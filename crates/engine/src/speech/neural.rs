//! Neural voices: a local voice program (Piper, or any program the user sets) or an
//! OpenAI-compatible cloud service makes a WAV file for each sentence, and the system's audio
//! player plays it.
//!
//! The cloud request goes through the system's `curl` (macOS, Windows 10 and later, Linux), so
//! WordCraft needs no HTTP or TLS library. The API key goes to `curl` on stdin, never on the
//! command line, and the sentence goes in a request file, never in an argument. The functions
//! that build the commands are plain, so the tests can check them on every system.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::settings::{CloudVoice, LocalVoice, endpoint_ok, env_name_ok};

/// Longest wait for one sentence from the cloud service, in seconds.
const CLOUD_TIMEOUT_SECS: &str = "60";

/// One program to run: the program, its arguments, what goes on its stdin and extra environment
/// variables.
#[derive(Debug, PartialEq)]
pub(crate) struct Run {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin: Vec<u8>,
    pub env: Vec<(String, String)>,
}

/// Piper's `--length_scale` for a speed: the length of the speech, the inverse of the speed.
pub(crate) fn length_scale(rate: f32) -> f32 {
    let r = if rate.is_finite() { rate.clamp(0.25, 4.0) } else { 1.0 };
    1.0 / r
}

/// The local voice program for one sentence: `voice` (the role's, else the default), the speed
/// and the WAV file to write fill the argument template; the sentence goes on stdin.
pub(crate) fn local_run(local: &LocalVoice, voice: &str, rate: f32, out: &Path, text: &str) -> Result<Run, String> {
    let voice = if voice.is_empty() { local.voice.as_str() } else { voice };
    if voice.is_empty() && local.args.iter().any(|a| a.contains("{voice}")) {
        return Err("Read Aloud needs a voice model for the local voice: set it in File › Options › Accessibility".into());
    }
    let out = out.to_string_lossy();
    let rate_s = format!("{:.2}", if rate.is_finite() { rate } else { 1.0 });
    let scale = format!("{:.3}", length_scale(rate));
    let args = local
        .args
        .iter()
        .map(|a| a.replace("{voice}", voice).replace("{output}", &out).replace("{rate}", &rate_s).replace("{lengthScale}", &scale))
        .collect();
    let mut stdin = text.as_bytes().to_vec();
    stdin.push(b'\n');
    Ok(Run { program: PathBuf::from(&local.program), args, stdin, env: Vec::new() })
}

/// Is `key` safe in a `curl` config line: printable ASCII without quotes or backslashes.
fn key_ok(key: &str) -> bool {
    !key.is_empty() && key.len() <= 512 && key.bytes().all(|b| (0x21..=0x7E).contains(&b) && b != b'"' && b != b'\\')
}

/// The request body for one sentence.
pub(crate) fn cloud_body(cloud: &CloudVoice, voice: &str, rate: f32, text: &str) -> Vec<u8> {
    let voice = if voice.is_empty() { cloud.voice.as_str() } else { voice };
    let speed = if rate.is_finite() { rate.clamp(0.25, 4.0) } else { 1.0 };
    json!({"model": cloud.model, "voice": voice, "input": text, "response_format": "wav", "speed": speed}).to_string().into_bytes()
}

/// `curl` posting the request file `body` to the cloud service, writing the WAV file `out`.
/// `key` is the API key from the environment variable the settings name.
pub(crate) fn cloud_run(cloud: &CloudVoice, key: Option<&str>, body: &Path, out: &Path) -> Result<Run, String> {
    if !endpoint_ok(&cloud.endpoint) {
        return Err("the cloud voice address must start with https://".into());
    }
    if !env_name_ok(&cloud.api_key_env) {
        return Err("the cloud voice needs the name of the environment variable that holds the API key".into());
    }
    let key = key
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or_else(|| format!("set the API key for the cloud voice in the {} environment variable", cloud.api_key_env))?;
    if !key_ok(key) {
        return Err(format!("the API key in {} has characters an API key can't have", cloud.api_key_env));
    }
    let args = [
        "-sS",
        "--fail",
        "--max-time",
        CLOUD_TIMEOUT_SECS,
        "-X",
        "POST",
        "--url",
        cloud.endpoint.as_str(),
        "-K",
        "-",
        "-H",
        "Content-Type: application/json",
        "--data-binary",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([format!("@{}", body.to_string_lossy()), "-o".into(), out.to_string_lossy().into_owned()])
    .collect();
    // The key goes in as a `curl` config line on stdin, so other programs can't see it in the
    // list of running processes.
    let stdin = format!("header = \"Authorization: Bearer {key}\"\n").into_bytes();
    Ok(Run { program: PathBuf::from("curl"), args, stdin, env: Vec::new() })
}

/// The programs that can play the WAV file `wav`, best first.
pub(crate) fn players(wav: &Path) -> Vec<Run> {
    let path = wav.to_string_lossy().into_owned();
    let run = |p: &str, args: &[&str]| Run {
        program: PathBuf::from(p),
        args: args.iter().map(|s| s.to_string()).chain([path.clone()]).collect(),
        stdin: Vec::new(),
        env: Vec::new(),
    };
    if cfg!(target_os = "macos") {
        vec![run("afplay", &[])]
    } else if cfg!(target_os = "windows") {
        // The path is in the environment, never in the script.
        vec![Run {
            program: super::windows_powershell(std::env::var_os("SystemRoot")),
            args: ["-NoProfile", "-NonInteractive", "-Command", "(New-Object System.Media.SoundPlayer $env:WORDCRAFT_WAV).PlaySync()"]
                .map(String::from)
                .to_vec(),
            stdin: Vec::new(),
            env: vec![("WORDCRAFT_WAV".into(), path)],
        }]
    } else {
        vec![run("pw-play", &[]), run("paplay", &[]), run("aplay", &["-q"]), run("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"])]
    }
}

/// A new temporary file name for Read Aloud audio or requests.
pub(crate) fn temp_file(ext: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("wordcraft-speech-{}-{n}.{ext}", std::process::id()))
}

/// How a program run ended.
#[derive(Debug, PartialEq)]
pub(crate) enum Ran {
    Exited(bool),
    Interrupted,
    NotStarted(String),
}

/// Run `run` to its end, or stop it when `changed()`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn run(run: &Run, changed: &dyn Fn() -> bool) -> Ran {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut c = Command::new(&run.program);
    c.args(&run.args).envs(run.env.iter().map(|(k, v)| (k, v)));
    #[cfg(target_os = "windows")]
    std::os::windows::process::CommandExt::creation_flags(&mut c, super::CREATE_NO_WINDOW);
    let mut child = match c.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(ch) => ch,
        Err(e) => return Ran::NotStarted(e.to_string()),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&run.stdin);
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ran::Exited(status.success()),
            Ok(None) => {}
            Err(e) => return Ran::NotStarted(e.to_string()),
        }
        if changed() {
            let _ = child.kill();
            let _ = child.wait();
            return Ran::Interrupted;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
}

/// Play the WAV file `wav` with the first player that starts.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn play(wav: &Path, changed: &dyn Fn() -> bool) -> Ran {
    let mut last = Ran::NotStarted("no audio player on this system (install PipeWire, PulseAudio, ALSA utilities or FFmpeg)".into());
    for p in players(wav) {
        match run(&p, changed) {
            Ran::NotStarted(e) => last = Ran::NotStarted(format!("{}: {e}", p.program.display())),
            done => return done,
        }
    }
    last
}

/// Delete a temporary file of Read Aloud's own (never a user's file: the names come from
/// [`temp_file`]).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn remove(path: impl AsRef<Path>) {
    let _ = std::fs::remove_file(path);
}

/// Write the request file of a cloud sentence (a [`temp_file`]).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn write_request(path: &Path, body: Vec<u8>) -> std::io::Result<()> {
    std::fs::write(path, body)
}

/// Does `path` hold some audio?
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn has_audio(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.len() > 44)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn piper_gets_the_model_speed_and_output() {
        let mut l = LocalVoice::default();
        let out = Path::new("/tmp/x.wav");
        assert!(local_run(&l, "", 1.0, out, "Hi").is_err(), "no model");
        l.voice = "/voices/en_US-amy.onnx".into();
        let r = local_run(&l, "", 2.0, out, "Hi").unwrap();
        assert_eq!(r.program, PathBuf::from("piper"));
        assert_eq!(r.args, ["--model", "/voices/en_US-amy.onnx", "--output_file", "/tmp/x.wav", "--length_scale", "0.500"]);
        assert_eq!(r.stdin, b"Hi\n");
        // The role's voice wins.
        assert_eq!(local_run(&l, "/voices/b.onnx", 1.0, out, "Hi").unwrap().args[1], "/voices/b.onnx");
        // A custom template without a voice needs no model.
        l.voice.clear();
        l.args = vec!["--out".into(), "{output}".into(), "--speed".into(), "{rate}".into()];
        assert_eq!(local_run(&l, "", 1.5, out, "x").unwrap().args, ["--out", "/tmp/x.wav", "--speed", "1.50"]);
    }

    #[test]
    fn cloud_request_keeps_the_key_and_text_off_the_command_line() {
        let c = CloudVoice::default();
        let body = Path::new("/tmp/b.json");
        let out = Path::new("/tmp/o.wav");
        assert!(cloud_run(&c, None, body, out).is_err());
        assert!(cloud_run(&c, Some("  "), body, out).is_err());
        assert!(cloud_run(&c, Some("sk-a\"b"), body, out).is_err());
        let r = cloud_run(&c, Some("sk-test123"), body, out).unwrap();
        assert_eq!(r.program, PathBuf::from("curl"));
        assert!(r.args.iter().all(|a| !a.contains("sk-test123")));
        assert!(r.args.windows(2).any(|w| w[0] == "--url" && w[1] == c.endpoint));
        assert!(r.args.iter().any(|a| a == "@/tmp/b.json"));
        assert_eq!(r.stdin, b"header = \"Authorization: Bearer sk-test123\"\n");
        let b: serde_json::Value = serde_json::from_slice(&cloud_body(&c, "", 9.0, "Hello \"there\"")).unwrap();
        assert_eq!(b["input"], "Hello \"there\"");
        assert_eq!(b["voice"], "alloy");
        assert_eq!(b["speed"], 4.0);
        assert_eq!(b["response_format"], "wav");
        let mut bad = c.clone();
        bad.endpoint = "http://example.com".into();
        assert!(cloud_run(&bad, Some("k"), body, out).is_err());
    }

    #[test]
    fn audio_players_get_the_file() {
        let p = players(Path::new("/tmp/a.wav"));
        assert!(!p.is_empty());
        for r in &p {
            assert!(r.args.last().is_some_and(|a| a == "/tmp/a.wav") || r.env.iter().any(|(_, v)| v == "/tmp/a.wav"));
        }
        assert_ne!(temp_file("wav"), temp_file("wav"));
        assert_eq!(length_scale(f32::NAN), 1.0);
        assert_eq!(length_scale(100.0), 0.25);
    }
}
