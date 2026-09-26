use crate::error::{LegError, Result};

pub(crate) struct SseEvent {
    pub(crate) name: String,
    pub(crate) data: String,
}

#[derive(Default)]
pub(crate) struct SseDecoder {
    line: Vec<u8>,
    event_name: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    pub(crate) fn push(
        &mut self,
        chunk: &[u8],
        on_event: &mut dyn FnMut(SseEvent) -> Result<()>,
    ) -> Result<()> {
        for byte in chunk {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.process_line(&line, on_event)?;
            } else {
                self.line.push(*byte);
            }
        }
        Ok(())
    }

    pub(crate) fn finish(
        &mut self,
        on_event: &mut dyn FnMut(SseEvent) -> Result<()>,
    ) -> Result<()> {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.process_line(&line, on_event)?;
        }
        self.dispatch(on_event)
    }

    fn process_line(
        &mut self,
        bytes: &[u8],
        on_event: &mut dyn FnMut(SseEvent) -> Result<()>,
    ) -> Result<()> {
        let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
        if bytes.is_empty() {
            return self.dispatch(on_event);
        }
        if bytes.first() == Some(&b':') {
            return Ok(());
        }

        let line = std::str::from_utf8(bytes)
            .map_err(|error| LegError::Decode(format!("invalid UTF-8 in SSE stream: {error}")))?;
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event_name = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, on_event: &mut dyn FnMut(SseEvent) -> Result<()>) -> Result<()> {
        if self.data.is_empty() {
            self.event_name = None;
            return Ok(());
        }
        let event = SseEvent {
            name: self
                .event_name
                .take()
                .unwrap_or_else(|| "message".to_string()),
            data: self.data.join("\n"),
        };
        self.data.clear();
        on_event(event)
    }
}
