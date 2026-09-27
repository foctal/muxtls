use tokio::sync::Semaphore;

use crate::{Error, Result};

/// Resource and protocol limits enforced per connection.
///
/// Call [`Limits::validate`] when limits originate in configuration. Endpoints
/// also validate limits before creating each connection.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Maximum graceful drain time before transport I/O is aborted.
    pub drain_timeout: std::time::Duration,
    /// Maximum queued DATA/FIN chunks, in addition to payload byte budgets.
    pub max_queued_outbound_frames: usize,
    /// Control frame capacity; at least three per open stream plus SETTINGS/PING.
    pub max_control_frames: usize,
    /// Maximum encoded inner frame size, excluding the four-byte length prefix.
    pub max_frame_size: usize,
    /// Maximum simultaneous open streams and queued incoming stream handles.
    pub max_open_streams: usize,
    /// Maximum total buffered inbound bytes per connection.
    pub max_inbound_connection_bytes: usize,
    /// Maximum total buffered outbound bytes per connection.
    pub max_outbound_connection_bytes: usize,
    /// Maximum buffered inbound bytes per stream.
    pub max_inbound_stream_bytes: usize,
    /// Maximum buffered outbound bytes per stream.
    pub max_outbound_stream_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            drain_timeout: std::time::Duration::from_secs(5),
            max_queued_outbound_frames: 1024,
            max_control_frames: 512,
            max_frame_size: 64 * 1024,
            max_open_streams: 128,
            max_inbound_connection_bytes: 4 * 1024 * 1024,
            max_outbound_connection_bytes: 4 * 1024 * 1024,
            max_inbound_stream_bytes: 256 * 1024,
            max_outbound_stream_bytes: 256 * 1024,
        }
    }
}

impl Limits {
    /// Smallest frame limit that can encode every control frame shape.
    pub const MIN_FRAME_SIZE: usize = 25;

    /// Validates that all limits are nonzero and representable by the runtime.
    pub fn validate(&self) -> Result<()> {
        if self.drain_timeout.is_zero() || self.drain_timeout > std::time::Duration::from_secs(3600)
        {
            return Err(invalid(
                "drain_timeout",
                "must be greater than zero and at most one hour",
            ));
        }
        if self.max_frame_size < Self::MIN_FRAME_SIZE {
            return Err(invalid(
                "max_frame_size",
                format!("must be at least {}", Self::MIN_FRAME_SIZE),
            ));
        }
        if self.max_frame_size > u32::MAX as usize {
            return Err(invalid(
                "max_frame_size",
                format!("must be at most {}", u32::MAX),
            ));
        }

        validate_semaphore("max_open_streams", self.max_open_streams)?;
        validate_semaphore(
            "max_queued_outbound_frames",
            self.max_queued_outbound_frames,
        )?;
        validate_semaphore("max_control_frames", self.max_control_frames)?;
        if self.max_queued_outbound_frames < self.max_open_streams {
            return Err(invalid(
                "max_queued_outbound_frames",
                "must allow at least one frame per admitted stream",
            ));
        }
        let minimum_controls = self
            .max_open_streams
            .checked_mul(3)
            .and_then(|n| n.checked_add(2))
            .ok_or_else(|| invalid("max_open_streams", "control capacity overflow"))?;
        if self.max_control_frames < minimum_controls {
            return Err(invalid(
                "max_control_frames",
                "must be at least 3 * max_open_streams + 2",
            ));
        }
        validate_byte_limit(
            "max_inbound_connection_bytes",
            self.max_inbound_connection_bytes,
        )?;
        validate_byte_limit(
            "max_outbound_connection_bytes",
            self.max_outbound_connection_bytes,
        )?;
        validate_byte_limit("max_inbound_stream_bytes", self.max_inbound_stream_bytes)?;
        validate_byte_limit("max_outbound_stream_bytes", self.max_outbound_stream_bytes)?;

        if self.max_inbound_stream_bytes > self.max_inbound_connection_bytes {
            return Err(invalid(
                "max_inbound_stream_bytes",
                "must not exceed max_inbound_connection_bytes",
            ));
        }
        if self.max_outbound_stream_bytes > self.max_outbound_connection_bytes {
            return Err(invalid(
                "max_outbound_stream_bytes",
                "must not exceed max_outbound_connection_bytes",
            ));
        }

        Ok(())
    }
}

fn validate_nonzero(field: &'static str, value: usize) -> Result<()> {
    if value == 0 {
        Err(invalid(field, "must be greater than zero"))
    } else {
        Ok(())
    }
}

fn validate_semaphore(field: &'static str, value: usize) -> Result<()> {
    validate_nonzero(field, value)?;
    if value > Semaphore::MAX_PERMITS {
        return Err(invalid(
            field,
            format!("must be at most {}", Semaphore::MAX_PERMITS),
        ));
    }
    Ok(())
}

fn validate_byte_limit(field: &'static str, value: usize) -> Result<()> {
    validate_semaphore(field, value)?;
    if value > u32::MAX as usize {
        return Err(invalid(field, format!("must be at most {}", u32::MAX)));
    }
    Ok(())
}

fn invalid(field: &'static str, reason: impl Into<String>) -> Error {
    Error::InvalidLimit {
        field,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::Limits;
    use crate::Error;

    #[test]
    fn defaults_are_valid() {
        Limits::default().validate().expect("default limits");
    }

    #[test]
    fn zero_and_inconsistent_limits_are_rejected() {
        let limits = Limits {
            max_open_streams: 0,
            ..Limits::default()
        };
        assert!(matches!(
            limits.validate(),
            Err(Error::InvalidLimit {
                field: "max_open_streams",
                ..
            })
        ));

        let limits = Limits {
            max_inbound_stream_bytes: Limits::default().max_inbound_connection_bytes + 1,
            ..Limits::default()
        };
        assert!(matches!(
            limits.validate(),
            Err(Error::InvalidLimit {
                field: "max_inbound_stream_bytes",
                ..
            })
        ));
    }
}
