/// Resource and protocol limits enforced per connection.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Maximum decoded inner frame payload size.
    pub max_frame_size: usize,
    /// Maximum simultaneous open streams.
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
            max_frame_size: 64 * 1024,
            max_open_streams: 128,
            max_inbound_connection_bytes: 4 * 1024 * 1024,
            max_outbound_connection_bytes: 4 * 1024 * 1024,
            max_inbound_stream_bytes: 256 * 1024,
            max_outbound_stream_bytes: 256 * 1024,
        }
    }
}
