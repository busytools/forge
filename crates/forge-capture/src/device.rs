//! The inputs a host can record from, and the plan one opens at.

use cpal::traits::{DeviceTrait, HostTrait};

use crate::Error;
use crate::SAMPLE_RATE;

/// An input the host can offer a user, and record from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable identity, and the value a host persists. cpal documents
    /// this as the thing to store and re-resolve, which is why the
    /// selection is keyed on it rather than on `name`: names collide
    /// between two identical interfaces and change when a user renames
    /// one, and neither should silently move the recording.
    pub id: String,
    /// Human label for a picker. Not an identity.
    pub name: String,
    /// Whether the system would pick this one when asked for no
    /// particular device.
    pub is_default: bool,
}

/// Every input the host could record from.
///
/// The crate never chooses: a caller that names nothing gets the system
/// default, and one that names a device gets that device or an error.
pub fn devices() -> Result<Vec<Device>, Error> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| d.id().ok());
    let found =
        host.input_devices().map_err(|error| Error::Capture { message: error.to_string() })?;

    Ok(found
        .filter_map(|device| {
            let id = device.id().ok()?;
            Some(record(&device, Some(&id), default.as_ref()))
        })
        .collect())
}

/// One device as the host describes it: the id a host persists, the label a
/// picker shows, and whether the system would pick it unaided.
fn record(
    device: &cpal::Device,
    id: Option<&cpal::DeviceId>,
    default: Option<&cpal::DeviceId>,
) -> Device {
    Device {
        is_default: id.is_some() && id == default,
        id: id.map(ToString::to_string).unwrap_or_default(),
        name: device.to_string(),
    }
}

/// The host's own record of a device already resolved: its id, its label,
/// and whether the system would have picked it unaided.
///
/// A capture carries this back to its caller, which is how a host names the
/// input the system default resolved to - the picker's list itself carries
/// no default mark. A device whose id cannot be read is still described: the
/// label is what a host draws, and refusing the take over an unreadable id
/// would lose the recording rather than just its name.
pub(crate) fn describe(device: &cpal::Device) -> Device {
    let default = cpal::default_host().default_input_device().and_then(|d| d.id().ok());
    record(device, device.id().ok().as_ref(), default.as_ref())
}

/// Resolve a caller's choice to a cpal device.
///
/// A named device that has gone is an error rather than a fallback:
/// someone who chose a USB interface and finds it unplugged needs to
/// know, not to be quietly recorded on the built-in microphone.
pub(crate) fn open_device(wanted: Option<&str>) -> Result<cpal::Device, Error> {
    let host = cpal::default_host();
    let Some(wanted) = wanted else {
        return host.default_input_device().ok_or(Error::NoInputDevice);
    };
    let found =
        host.input_devices().map_err(|error| Error::Capture { message: error.to_string() })?;
    for device in found {
        if device.id().is_ok_and(|id| id.to_string() == wanted) {
            return Ok(device);
        }
    }
    Err(Error::DeviceNotFound {
        wanted: wanted.to_owned(),
        available: devices()?
            .into_iter()
            .map(|d| format!("{} ({})", d.id, d.name))
            .collect::<Vec<_>>()
            .join(", "),
    })
}

/// How a capture opens: the stream config the device will run at, and the
/// rate its samples arrive at when that is not already [`SAMPLE_RATE`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InputPlan {
    pub(crate) config: cpal::StreamConfig,
    /// `Some` when the device cannot offer [`SAMPLE_RATE`] itself and
    /// its audio needs converting; `None` when the samples flow
    /// through untouched.
    pub(crate) resample_from: Option<u32>,
}

/// Pick an input the models can read.
///
/// A device offering [`SAMPLE_RATE`] in F32 opens natively, fewest
/// channels winning: less to average, less to go wrong. One that does
/// not still opens its best F32 config - fewest channels, then the
/// rate nearest the model rate - and its audio is downmixed and
/// resampled with a real filter downstream. Measured on the target
/// hardware, which offers 16, 24 and 32 kHz and never fewer than two
/// channels - so requiring mono here would have meant no microphone at
/// all - and on USB microphones that offer only 48 kHz.
///
/// What is still refused is a device with no F32 config at all: the
/// error names everything it offered, so the mismatch is legible.
pub(crate) fn input_config(device: &cpal::Device) -> Result<InputPlan, Error> {
    let supported = device
        .supported_input_configs()
        .map_err(|error| Error::Capture { message: error.to_string() })?;
    input_plan(supported)
}

/// The negotiation over synthetic ranges, so a device is not needed to
/// test it.
fn input_plan(
    supported: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
) -> Result<InputPlan, Error> {
    let wanted: cpal::SampleRate = SAMPLE_RATE;
    let mut offered = Vec::new();
    let mut native: Option<cpal::StreamConfig> = None;
    // (channels, distance from the model rate, rate, config)
    let mut fallback: Option<(u16, u32, cpal::SampleRate, cpal::StreamConfig)> = None;
    for range in supported {
        offered.push(format!(
            "{}ch {}-{}Hz {:?}",
            range.channels(),
            range.min_sample_rate(),
            range.max_sample_rate(),
            range.sample_format()
        ));
        if range.sample_format() != cpal::SampleFormat::F32 {
            continue;
        }
        if let Some(config) = range.try_with_sample_rate(wanted) {
            let config: cpal::StreamConfig = config.into();
            if native.as_ref().is_none_or(|b| config.channels < b.channels) {
                native = Some(config);
            }
            continue;
        }
        // The range cannot serve the model rate itself, so its nearest
        // end is the resample candidate.
        let rate = SAMPLE_RATE.clamp(range.min_sample_rate(), range.max_sample_rate());
        let candidate = (
            range.channels(),
            rate.abs_diff(SAMPLE_RATE),
            rate,
            cpal::StreamConfig {
                channels: range.channels(),
                sample_rate: rate,
                buffer_size: cpal::BufferSize::Default,
            },
        );
        let better = match fallback {
            None => true,
            Some((channels, distance, _, _)) => {
                candidate.0 < channels || (candidate.0 == channels && candidate.1 < distance)
            }
        };
        if better {
            fallback = Some(candidate);
        }
    }
    if let Some(config) = native {
        return Ok(InputPlan { config, resample_from: None });
    }
    fallback
        .map(|(_, _, rate, config)| InputPlan { config, resample_from: Some(rate) })
        .ok_or(Error::UnsupportedInput { wanted: SAMPLE_RATE, offered: offered.join(", ") })
}

#[cfg(test)]
mod tests_input_config {
    use super::*;
    use cpal::{SampleFormat, SupportedBufferSize, SupportedStreamConfigRange};

    /// A cpal range with no device behind it. The negotiation reads
    /// only these fields, so this is a complete synthetic device.
    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, min, max, SupportedBufferSize::Unknown, format)
    }

    #[test]
    fn a_device_offering_16k_takes_the_native_path() {
        // The Mac's built-in input measured inventory: 16 kHz exists,
        // but only in stereo beside 24 and 32.
        let plan = input_plan([
            range(2, 16_000, 16_000, SampleFormat::F32),
            range(2, 24_000, 24_000, SampleFormat::F32),
            range(2, 32_000, 32_000, SampleFormat::F32),
        ])
        .expect("a 16 kHz config is on offer");
        assert_eq!(
            plan.config.sample_rate, SAMPLE_RATE,
            "the native path must open at the model rate"
        );
        assert_eq!(
            plan.resample_from, None,
            "a native 16 kHz config must not engage the resampler"
        );
    }

    #[test]
    fn a_device_without_16k_falls_back_to_its_best_f32_offer() {
        // The AirHug 28's whole inventory, the device this case exists for.
        let plan = input_plan([range(2, 48_000, 48_000, SampleFormat::F32)])
            .expect("an F32 offer must be usable, not refused");
        assert_eq!(
            (plan.config.channels, plan.config.sample_rate),
            (2, 48_000),
            "the fallback opens the device's own best F32 config"
        );
        assert_eq!(
            plan.resample_from,
            Some(48_000),
            "the samples arrive at 48 kHz and must be converted to the model rate"
        );
    }

    #[test]
    fn the_fallback_prefers_fewer_channels_then_the_rate_nearest_16k() {
        let plan = input_plan([
            range(2, 24_000, 24_000, SampleFormat::F32),
            range(1, 44_100, 44_100, SampleFormat::F32),
        ])
        .expect("offers exist");
        assert_eq!(
            (plan.config.channels, plan.config.sample_rate),
            (1, 44_100),
            "fewest channels wins even when its rate is the farther one"
        );

        let plan = input_plan([
            range(2, 48_000, 48_000, SampleFormat::F32),
            range(2, 44_100, 44_100, SampleFormat::F32),
        ])
        .expect("offers exist");
        assert_eq!(
            plan.config.sample_rate, 44_100,
            "equal channels: the rate nearest the model rate wins"
        );
    }

    #[test]
    fn a_device_with_no_f32_offer_at_all_is_still_refused_with_what_it_offers() {
        let err =
            input_plan([range(2, 44_100, 44_100, SampleFormat::I16)]).expect_err("no F32, no open");
        match err {
            Error::UnsupportedInput { wanted, offered } => {
                assert_eq!(wanted, SAMPLE_RATE);
                assert!(
                    offered.contains("2ch 44100-44100Hz"),
                    "the refusal must keep naming what the device offered, got: {offered}"
                );
            }
            other => panic!("the no-F32 refusal must stay UnsupportedInput, got: {other:?}"),
        }
    }

    #[test]
    fn an_f32_offer_wins_even_when_a_non_f32_range_serves_16k() {
        // Only the F32 stream can be opened, so a native 16 kHz I16
        // range must not beat an F32 range that needs conversion.
        let plan = input_plan([
            range(1, 16_000, 16_000, SampleFormat::I16),
            range(2, 48_000, 48_000, SampleFormat::F32),
        ])
        .expect("an F32 offer exists");
        assert_eq!(
            plan.resample_from,
            Some(48_000),
            "the F32 offer is the only one this crate can open"
        );
        assert_eq!((plan.config.channels, plan.config.sample_rate), (2, 48_000));
    }

    #[test]
    #[ignore = "enumerates the real audio devices; run with --run-ignored all on a machine with audio hardware"]
    fn enumeration_names_at_most_one_default() {
        // No audio hardware in CI, so an empty list is a valid answer;
        // what must never happen is two devices both claiming default,
        // which would make a host's picker ambiguous.
        let Ok(found) = devices() else { return };
        let defaults = found.iter().filter(|d| d.is_default).count();
        assert!(defaults <= 1, "at most one input can be the system default, found {defaults}");
        for device in &found {
            assert!(
                !device.id.is_empty(),
                "a device with no id cannot be persisted or re-resolved"
            );
        }
    }

    #[test]
    #[ignore = "enumerates the real audio devices; run with --run-ignored all on a machine with audio hardware"]
    fn a_named_device_that_is_absent_never_falls_back() {
        // Skipped where there is no audio stack at all, since then the
        // absence proves nothing about the resolution path.
        // Guarded on a DEFAULT existing, not merely on the list being
        // non-empty: the second assertion needs `default_input_device()`
        // to be Some, which is a different cpal call.
        let Ok(found) = devices() else { return };
        if !found.iter().any(|d| d.is_default) {
            return;
        }
        assert!(
            open_device(Some("forge-capture-no-such-device")).is_err(),
            "a named device that is not present must error, never quietly record on the default"
        );
        assert!(
            open_device(None).is_ok(),
            "naming nothing must still resolve to the system default"
        );
    }
}
