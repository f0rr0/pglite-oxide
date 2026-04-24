use crate::protocol::string_utils::byte_length_utf8;

const DEFAULT_SIZE: usize = 256;

#[derive(Clone, Debug)]
pub struct BufferWriter {
    buffer: Vec<u8>,
    size: usize,
    offset: usize,
}

impl Default for BufferWriter {
    fn default() -> Self {
        Self::new(DEFAULT_SIZE)
    }
}

impl BufferWriter {
    pub fn new(size: usize) -> Self {
        let buffer = vec![0; size];
        // reserve header space (code + len = 5 bytes)
        Self {
            buffer,
            size,
            offset: 5,
        }
    }

    fn ensure_capacity(&mut self, additional: usize) {
        if self.buffer.len() - self.offset < additional {
            let old_len = self.buffer.len();
            // Exponential growth ~1.5x as in TS implementation.
            let mut new_len = old_len + (old_len >> 1) + additional;
            if new_len == old_len {
                new_len += additional;
            }
            self.buffer.resize(new_len, 0);
        }
    }

    fn write_be_bytes(&mut self, bytes: &[u8]) {
        let len = bytes.len();
        self.ensure_capacity(len);
        self.buffer[self.offset..self.offset + len].copy_from_slice(bytes);
        self.offset += len;
    }

    pub fn add_int32(&mut self, value: i32) -> &mut Self {
        self.write_be_bytes(&value.to_be_bytes());
        self
    }

    pub fn add_int16(&mut self, value: i16) -> &mut Self {
        self.write_be_bytes(&value.to_be_bytes());
        self
    }

    pub fn add_cstring(&mut self, value: &str) -> &mut Self {
        if !value.is_empty() {
            self.add_string(value);
        }
        self.add_bytes(&[0]);
        self
    }

    pub fn add_string(&mut self, value: &str) -> &mut Self {
        let length = byte_length_utf8(value);
        self.ensure_capacity(length);
        let end = self.offset + length;
        self.buffer[self.offset..end].copy_from_slice(value.as_bytes());
        self.offset = end;
        self
    }

    pub fn add_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.write_be_bytes(bytes);
        self
    }

    fn join(&mut self, code: Option<u8>) -> Vec<u8> {
        if let Some(code) = code {
            self.buffer[0] = code;
            let length = (self.offset - 1) as i32;
            self.buffer[1..5].copy_from_slice(&length.to_be_bytes());
        }
        let start = if code.is_some() { 0 } else { 5 };
        self.buffer[start..self.offset].to_vec()
    }

    pub fn flush(&mut self, code: Option<u8>) -> Vec<u8> {
        let result = self.join(code);
        self.buffer = vec![0; self.size];
        self.offset = 5;
        result
    }
}
