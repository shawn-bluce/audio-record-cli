mod agc;
mod audio;
mod convert;
mod coreaudio;
mod encoder;
mod finalize;
mod journal;
mod m4a;
mod meter;
mod output;
mod power;
mod resample;
mod session;
mod silence;
mod ui;
mod writer;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use clap::Parser;

use encoder::{AAC_BITRATE, AacEncoder};
use finalize::Remuxed;
use journal::Journal;
use meter::{Levels, StatusLine};
use power::Power;
use session::{Session, Supervisor};
use ui::{fmt_frames, paint};

const AFTER_HELP: &str = "\
Environment:
  AREC_OUTPUT_DIR  Output directory. Defaults to the current directory; if it is not
                   writable, a warning is printed and the current directory is used.

Output files:
  <start>_<end>.m4a     the recording (AAC-LC, 24 kHz, stereo, 64 kbps)
  <start>_<end>_c.m4a   only if there are silences of 5s or more: a copy without them

While recording, audio is streamed to <start>_recording.aac, which stays playable
if arec is killed or crashes; the next run converts such leftovers automatically.

Recording follows the system default input/output devices and reconnects
automatically when they change. Idle sleep is prevented while recording; closing
the lid or putting the Mac to sleep stops and saves the recording.
Press Ctrl+C to stop.";

#[derive(Parser)]
#[command(
    name = "arec",
    version,
    about = "Record macOS microphone + system audio into one stereo AAC/M4A file \
             (left = microphone, right = system audio)",
    after_help = AFTER_HELP
)]
struct Cli {}

const UI_TICK: Duration = Duration::from_millis(100);
/// Print diagnostics every this many UI ticks when `AREC_DEBUG` is set.
const DEBUG_EVERY_TICKS: u64 = 10;

fn main() {
    Cli::parse();
    if let Err(e) = run() {
        ui::error(format_args!("{e:#}"));
        std::process::exit(1);
    }
}

fn now_ts() -> String {
    output::timestamp(SystemTime::now())
}

fn run() -> Result<()> {
    let out_dir = output::resolve_output_dir()?;
    recover_orphans(&out_dir);
    let stop = stop_on_signals()?;

    let (tx, rx) = mpsc::channel();
    let mut devices = Supervisor::new(tx)?;
    let start = now_ts();
    let journal_path = output::journal_path(&out_dir, &start);
    let encoder = AacEncoder::new()?;
    let journal = Journal::create(&journal_path)?;
    let mut power = Power::new();
    print_spec(devices.session(), &out_dir, &journal_path, &power);

    let levels = Arc::new(Levels::default());
    let writer_done = Arc::new(AtomicBool::new(false));
    let writer = {
        let (levels, done, stop) = (levels.clone(), writer_done.clone(), stop.clone());
        thread::Builder::new()
            .name("arec-writer".into())
            .spawn(move || {
                let r = writer::run(rx, encoder, journal, &levels, &done);
                if r.is_err() {
                    stop.store(true, Ordering::SeqCst); // abort recording on disk errors
                }
                r
            })
            .context("Starting the writer thread")?
    };

    if let Err(e) = devices.start() {
        drop(devices);
        writer_done.store(true, Ordering::SeqCst);
        let _ = writer.join();
        let _ = std::fs::remove_file(&journal_path);
        return Err(e);
    }

    let mut status = StatusLine::new();
    let debug = std::env::var_os("AREC_DEBUG").is_some();
    let mut ticks = 0u64;
    // The recording really starts with the first buffer; name the files after that moment.
    let mut audio_start: Option<String> = None;
    let stop_reason = loop {
        if stop.load(Ordering::SeqCst) {
            break "Stopping";
        }
        if let Some(reason) = power.stop_reason() {
            break reason;
        }
        thread::sleep(UI_TICK);
        ticks += 1;
        if audio_start.is_none() && devices.has_audio() {
            audio_start = Some(now_ts());
            status.mark_started();
        }
        devices.poll(&mut status);
        status.tick(&levels, devices.dropped_frames());
        if debug && ticks.is_multiple_of(DEBUG_EVERY_TICKS) {
            status.message(&devices.debug_line());
        }
    };

    // ---- Shutdown: stop IO first, then let the writer drain everything and flush the journal.
    let end = now_ts();
    status.clear();
    eprintln!("{stop_reason} and saving…");
    let dropped = devices.dropped_frames();
    drop(devices);
    writer_done.store(true, Ordering::SeqCst);
    let (frames, journal_lock) = writer
        .join()
        .unwrap_or_else(|_| Err(anyhow!("The writer thread crashed")))
        .context("Writing audio failed")
        .unwrap_or_else(|e| abort_keeping_journal(e, &journal_path));
    if frames == 0 {
        drop(journal_lock);
        let _ = std::fs::remove_file(&journal_path);
        eprintln!("No audio was recorded; nothing saved.");
        return Ok(());
    }

    let start = audio_start.unwrap_or(start);
    let headline = if dropped > 0 {
        format!("✔ Recording saved ({dropped} frames dropped)")
    } else {
        "✔ Recording saved".to_string()
    };
    let rec = save(
        &journal_path,
        &output::final_path(&out_dir, &start, &end),
        Some(frames),
        &headline,
    );
    drop(journal_lock);
    let rec = rec.unwrap_or_else(|e| abort_keeping_journal(e, &journal_path));
    // The recording is safe on disk: a pending system sleep may go ahead now.
    power.release();
    let compact = compact(&rec);

    // Plain paths on stdout so the result can be piped into other tools.
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        println!("{}", rec.path.display());
        if let Some(c) = compact {
            println!("{}", c.display());
        }
    }
    Ok(())
}

/// Ctrl+C / SIGTERM / SIGHUP set the returned flag (graceful stop). A third
/// Ctrl+C exits immediately.
fn stop_on_signals() -> Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let presses = AtomicUsize::new(0);
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::SeqCst);
        match presses.fetch_add(1, Ordering::SeqCst) + 1 {
            1 => {}
            2 => eprintln!(
                "{}Finishing up, please wait… (press Ctrl+C again to force quit; \
                 the .aac file keeps everything recorded so far)",
                if *ui::TTY { "\r\x1b[2K" } else { "" }
            ),
            _ => std::process::exit(130),
        }
    })
    .context("Installing the Ctrl+C handler")?;
    Ok(stop)
}

fn abort_keeping_journal(e: anyhow::Error, journal: &Path) -> ! {
    ui::error(format_args!("{e:#}"));
    eprintln!(
        "Everything recorded so far is in {} (playable; the next run of arec converts it to .m4a).",
        journal.display()
    );
    std::process::exit(1);
}

/// Remux `journal` into `dst` and report it under `headline`.
fn save(journal: &Path, dst: &Path, frames: Option<u64>, headline: &str) -> Result<Remuxed> {
    let rec = finalize::remux(journal, dst, frames)
        .with_context(|| format!("Saving {} failed", dst.display()))?;
    eprintln!(
        "{}  duration {}",
        paint("32", headline),
        fmt_frames(rec.frames)
    );
    ui::print_file(&rec.path, "");
    Ok(rec)
}

/// Write and report the `_c` copy of `rec`, if it has long silences.
fn compact(rec: &Remuxed) -> Option<std::path::PathBuf> {
    eprintln!(
        "Looking for silences of {}s or more…",
        silence::MIN_SILENCE_SECS
    );
    match finalize::compact(rec) {
        Ok(Some(c)) => {
            let s = if c.cuts == 1 { "" } else { "s" };
            let note = format!(
                ", {} — {} silence{s} shortened",
                fmt_frames(c.frames),
                c.cuts
            );
            ui::print_file(&c.path, &note);
            Some(c.path)
        }
        Ok(None) => {
            eprintln!("  none found, no _c copy needed");
            None
        }
        Err(e) => {
            ui::warning(format_args!("could not create the compact copy: {e:#}"));
            None
        }
    }
}

/// Convert journals left behind by a killed or crashed `arec` into normal recordings.
fn recover_orphans(dir: &Path) {
    for orphan in finalize::orphans(dir) {
        let dst = output::final_path(dir, &orphan.start, &orphan.end);
        match save(
            &orphan.journal,
            &dst,
            None,
            "✔ Recovered an interrupted recording",
        ) {
            Ok(rec) => {
                compact(&rec);
            }
            Err(e) => ui::warning(format_args!(
                "could not recover {}: {e:#}",
                orphan.journal.display()
            )),
        }
        eprintln!();
    }
}

fn print_spec(session: Option<&Session>, dir: &Path, journal: &Path, power: &Power) {
    let b = |s: &str| paint("1", s);
    eprintln!("{}", b("arec · recording"));
    eprintln!(
        "  Format      AAC-LC · {} kbps · stereo · {} kHz · M4A",
        AAC_BITRATE / 1000,
        audio::SAMPLE_RATE / 1000.0
    );
    eprintln!("  Channels    left = microphone, right = system audio");
    eprintln!(
        "  Processing  per-channel auto gain (target {} dBFS) + peak limiter",
        agc::TARGET_DB
    );
    if let Some(s) = session {
        eprintln!("  Capture     {} Hz, Float32", s.rate());
        eprintln!("  Microphone  {}", s.mic.name);
        eprintln!("  System      all apps (output: {})", s.output.name);
    }
    eprintln!("  Devices     follow system defaults, auto-reconnect on change");
    eprintln!("  Power       {}", power.summary());
    eprintln!(
        "  Live file   {} (crash-safe, becomes .m4a on stop)",
        journal.file_name().unwrap_or_default().to_string_lossy()
    );
    eprintln!("  Output dir  {}", dir.display());
    eprintln!("  Press {} to stop\n", b("Ctrl+C"));
}
