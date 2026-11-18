use anyhow::Result;
use std::{io::BufWriter, path::Path};

pub fn export_wav(
    path: &Path,
    rate: u32,
    channels: u16,
    samples: impl IntoIterator<Item = f32>,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    let mut wav = hound::WavWriter::new(
        BufWriter::new(file.as_file_mut()),
        hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )?;
    for sample in samples {
        wav.write_sample(sample)?;
    }
    wav.finalize()?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}
