use anyhow::{Context, Result, bail};
use pipewire as pw;
use pw::{properties::properties, spa};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub const RATE: u32 = 16_000;
const MAX_BYTES: usize = RATE as usize * 2 * 120;
pub const MAX_WAV_BYTES: usize = MAX_BYTES + 44;

/// Recovery accepts the exact bounded WAV format produced by native capture.
pub fn validate_wav(bytes: &[u8]) -> Result<()> {
    if bytes.len() < 46 || bytes.len() > MAX_WAV_BYTES || !(bytes.len() - 44).is_multiple_of(2) {
        bail!("Повреждённая или слишком длинная аудиозапись");
    }
    let mut header = wav(&[]);
    header[4..8].copy_from_slice(&((bytes.len() - 8) as u32).to_le_bytes());
    header[40..44].copy_from_slice(&((bytes.len() - 44) as u32).to_le_bytes());
    if bytes[..44] != header {
        bail!("Неподдерживаемый формат сохранённой аудиозаписи");
    }
    Ok(())
}

/// Native PipeWire capture. Two-minute cap keeps this integration spike bounded.
pub fn capture(stop: Arc<AtomicBool>, limit: Duration) -> Result<Vec<u8>> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "Velo native microphone",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Communication",
        },
    )?;
    let pcm = Rc::new(RefCell::new(Vec::with_capacity(RATE as usize * 2 * 10)));
    let problem = Rc::new(RefCell::new(None::<String>));
    let output = pcm.clone();
    let failure = problem.clone();
    let format_failure = problem.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_, _, _, state| {
            if let pw::stream::StreamState::Error(error) = state {
                *failure.borrow_mut() = Some(error.to_string());
            }
        })
        .param_changed(move |_, _, id, pod| {
            if id == spa::param::ParamType::Format.as_raw()
                && let Some(pod) = pod
            {
                let mut info = spa::param::audio::AudioInfoRaw::new();
                if info.parse(pod).is_err()
                    || info.rate() != RATE
                    || info.channels() != 1
                    || info.format() != spa::param::audio::AudioFormat::S16LE
                {
                    *format_failure.borrow_mut() =
                        Some("PipeWire negotiated an unexpected audio format".into());
                }
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            if let Some(bytes) = data.data() {
                let end = offset.saturating_add(size).min(bytes.len());
                if offset <= end {
                    let mut pcm = output.borrow_mut();
                    let count = (end - offset).min(MAX_BYTES.saturating_sub(pcm.len()));
                    pcm.extend_from_slice(&bytes[offset..offset + count]);
                }
            }
        })
        .register()?;
    let mut format = spa::param::audio::AudioInfoRaw::new();
    format.set_format(spa::param::audio::AudioFormat::S16LE);
    format.set_rate(RATE);
    format.set_channels(1);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: format.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .context("Serialize PipeWire audio format")?
    .0
    .into_inner();
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [spa::pod::Pod::from_bytes(&bytes).context("PipeWire format")?],
    )?;
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed)
        && started.elapsed() < limit
        && pcm.borrow().len() < MAX_BYTES
    {
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(40)));
        if let Some(error) = problem.borrow_mut().take() {
            bail!("PipeWire: {error}")
        }
    }
    stream.disconnect()?;
    let mut result = std::mem::take(&mut *pcm.borrow_mut());
    result.truncate(result.len() / 2 * 2);
    if result.len() < RATE as usize / 5 {
        bail!("Recording too short or no microphone data")
    }
    Ok(result)
}

pub fn wav(pcm: &[u8]) -> Vec<u8> {
    let n = pcm.len() as u32;
    let mut out = Vec::with_capacity(pcm.len() + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + n).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn wav_header_matches_pcm() {
        let wav = super::wav(&vec![0; 32000]);
        assert_eq!(wav.len(), 32044);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 32000);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
    }
}
