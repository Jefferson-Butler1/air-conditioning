//! The slice of the Apple SMC this tool needs: fan control and die temperatures.

use std::ops::Range;
use std::thread::sleep;
use std::time::Duration;

use smc_lib::io::{IOService, err_str};
use smc_lib::value::SmcValue;

use crate::Result;

type Key = [u8; 4];

const FAN_COUNT: Key = *b"FNum";
/// M3 and M4 Macs ignore manual fan modes until this is set; older chips don't have it.
const FAN_UNLOCK: Key = *b"Ftst";
const AUTO: u8 = 0;
const MANUAL: u8 = 1;

/// CPU and GPU die sensors. The exact keys change with every chip generation; these prefixes haven't.
const DIE_SENSOR_PREFIXES: [&[u8]; 4] = [b"Tp", b"Te", b"Tf", b"Tg"];
/// Unpopulated sensors read 0, so anything outside this range isn't a die temperature.
const PLAUSIBLE_CELSIUS: Range<f32> = 1.0..150.0;

/// The SMC applies a write a moment after accepting it: about 100 ms on an M1 Max,
/// and reportedly several seconds of repeated writes on an unlocked M3 or M4.
const APPLY_POLL: Duration = Duration::from_millis(100);
const APPLY_POLLS: u32 = 100;

pub struct Smc(IOService);

impl Smc {
    pub fn open() -> Result<Self> {
        let service = IOService::init().map_err(|e| format!("can't reach the SMC: {e}"))?;
        Ok(Self(service))
    }

    fn read(&self, key: Key) -> Result<SmcValue> {
        let value = self
            .0
            .read_key(&key)
            .map_err(|e| format!("can't read {}: {}", name(key), err_str(e)))?;
        let value = value
            .data_value()
            .ok_or_else(|| format!("{} has a type this tool doesn't know", name(key)))?;
        Ok(value)
    }

    fn read_f32(&self, key: Key) -> Result<f32> {
        match self.read(key)? {
            SmcValue::F32 { le, .. } => Ok(le),
            _ => Err(format!("{} isn't a float on this Mac", name(key)).into()),
        }
    }

    fn read_u8(&self, key: Key) -> Result<u8> {
        match self.read(key)? {
            SmcValue::U8(value) => Ok(value),
            _ => Err(format!("{} isn't a byte on this Mac", name(key)).into()),
        }
    }

    fn has(&self, key: Key) -> bool {
        self.0.get_key_info(&key).is_ok()
    }

    /// smc-lib doesn't surface the SMC's own verdict on a write, so keep writing until a read-back shows it took.
    fn write(&self, key: Key, bytes: &[u8], applied: impl Fn(SmcValue) -> bool) -> Result<()> {
        for _ in 0..APPLY_POLLS {
            self.0
                .write_key(&key, bytes)
                .map_err(|e| format!("can't write {}: {}", name(key), err_str(e)))?;
            sleep(APPLY_POLL);
            // A failed read-back is just one more reason to try again.
            if self.read(key).is_ok_and(&applied) {
                return Ok(());
            }
        }
        Err(format!("the SMC ignored a write to {}", name(key)).into())
    }

    fn unlock_fans(&self) -> Result<()> {
        if !self.has(FAN_UNLOCK) {
            return Ok(());
        }
        self.write(FAN_UNLOCK, &[1], |unlock| unlock == SmcValue::U8(1))
    }

    /// Hands every fan back to macOS.
    pub fn release_fans(&self) -> Result<()> {
        let count = self.read_u8(FAN_COUNT)?;
        // Not `try_for_each`: one stubborn fan mustn't leave the others stuck in manual.
        let fans = (0..count)
            .map(|fan| {
                // Some Macs report a third, system-managed mode here; all that matters is leaving manual.
                self.write(fan_key(fan, *b"Md"), &[AUTO], |mode| {
                    mode != SmcValue::U8(MANUAL)
                })
            })
            .fold(Ok(()), Result::and);
        let relock = if self.has(FAN_UNLOCK) {
            self.write(FAN_UNLOCK, &[0], |unlock| unlock == SmcValue::U8(0))
        } else {
            Ok(())
        };
        fans.and(relock)
    }

    /// Finds the CPU and GPU die sensors. Walks every SMC key, which takes a second or two.
    pub fn thermometer(&self) -> Result<Thermometer<'_>> {
        let keys = self
            .0
            .values_iter()
            .map_err(|e| format!("can't list SMC keys: {}", err_str(e)))?
            .filter_map(std::result::Result::ok)
            .filter(|value| DIE_SENSOR_PREFIXES.contains(&&value.key[..2]))
            .filter(|value| matches!(value.data_value(), Some(SmcValue::F32 { le, .. }) if PLAUSIBLE_CELSIUS.contains(&le)))
            .map(|value| value.key)
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return Err(
                "no CPU or GPU temperature sensors found, so --target can't work on this Mac"
                    .into(),
            );
        }
        Ok(Thermometer { smc: self, keys })
    }
}

pub struct Thermometer<'a> {
    smc: &'a Smc,
    keys: Vec<Key>,
}

impl Thermometer<'_> {
    /// The hottest CPU or GPU die reading in °C, or `None` if no sensor answered.
    pub fn hottest(&self) -> Option<f32> {
        self.keys
            .iter()
            .filter_map(|&key| self.smc.read_f32(key).ok())
            .filter(|celsius| PLAUSIBLE_CELSIUS.contains(celsius))
            .reduce(f32::max)
    }
}

/// Fans held at max. Dropping this without `release` still hands them back, so errors and panics can't strand them.
pub struct MaxedFans<'a> {
    smc: &'a Smc,
    count: u8,
    released: bool,
}

impl<'a> MaxedFans<'a> {
    pub fn start(smc: &'a Smc) -> Result<Self> {
        let count = match smc.read_u8(FAN_COUNT)? {
            0 => return Err("this Mac has no fans".into()),
            // Fan keys carry a single digit.
            count @ 1..=9 => count,
            count => return Err(format!("unexpected fan count: {count}").into()),
        };
        // Built before the first write so that failing partway still hands the fans back.
        let fans = Self {
            smc,
            count,
            released: false,
        };
        smc.unlock_fans()?;
        for fan in 0..count {
            let max = smc.read_f32(fan_key(fan, *b"Mx"))?;
            smc.write(fan_key(fan, *b"Md"), &[MANUAL], |mode| {
                mode == SmcValue::U8(MANUAL)
            })?;
            smc.write(
                fan_key(fan, *b"Tg"),
                &max.to_le_bytes(),
                |target| matches!(target, SmcValue::F32 { le, .. } if (le - max).abs() < 1.0),
            )?;
        }
        Ok(fans)
    }

    /// False once anything else (macOS, another tool, `air-conditioning off`) has taken a fan out of manual.
    pub fn still_held(&self) -> Result<bool> {
        for fan in 0..self.count {
            if self.smc.read_u8(fan_key(fan, *b"Md"))? != MANUAL {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn release(mut self) -> Result<()> {
        self.released = true;
        self.hand_back()
    }

    fn hand_back(&self) -> Result<()> {
        self.smc.release_fans().map_err(|e| {
            format!(
                "couldn't hand the fans back to macOS ({e}); run `air-conditioning off` or reboot"
            )
            .into()
        })
    }
}

impl Drop for MaxedFans<'_> {
    fn drop(&mut self) {
        if !self.released
            && let Err(e) = self.hand_back()
        {
            eprintln!("error: {e}");
        }
    }
}

fn fan_key(fan: u8, field: [u8; 2]) -> Key {
    [b'F', b'0' + fan, field[0], field[1]]
}

fn name(key: Key) -> String {
    String::from_utf8_lossy(&key).into_owned()
}
