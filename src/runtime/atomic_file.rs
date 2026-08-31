use std::io::{self, Write};
use std::path::Path;

use atomic_write_file::AtomicWriteFile;

pub(crate) fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(bytes)?;
    file.commit()
}
