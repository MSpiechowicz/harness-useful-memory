pub trait Sink {
    fn write(&self, line: &str);
}

pub struct FileSink;

impl Sink for FileSink {
    fn write(&self, line: &str) {
        flush_to_disk(line);
    }
}

pub struct NullSink;

impl Sink for NullSink {
    fn write(&self, _line: &str) {}
}

pub fn log_line(sink: &dyn Sink, line: &str) {
    sink.write(line);
}

fn flush_to_disk(line: &str) {
    let _ = line;
}
