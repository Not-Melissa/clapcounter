use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, InputDevices, Sample, SizedSample,
};
use gag::Gag;
use inquire::{CustomType, InquireError};
use std::{
    process,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const ACTIVE_DELAY: Duration = Duration::from_secs(1);
const CALIBRATION_COMPLETE_DELAY: Duration = Duration::from_secs(2);

const BASELINE_WINDOW: usize = 50;
const FREQUENCY: f32 = 50.0;

const PEAK_THRESHOLD: f32 = 15.0;
const RESET_THRESHOLD: f32 = 4.0;
const RESET_DISTANCE: isize = 8;

const CALIBRATION_TOLERANCE: f32 = 0.9;

fn main() -> anyhow::Result<()> {
    let err_gag = Gag::stderr()?;

    // Audio setup
    let host = cpal::default_host();
    let input_devs = get_input_devices(&host)?;
    println!("Input devices:");
    for (i, dev) in input_devs.enumerate() {
        println!("[{i}] {}", dev.name()?);
    }

    let input_devs = get_input_devices(&host)?;
    let default_index = host.default_input_device().and_then(|def| {
        let def_name = def.name().ok()?;
        input_devs
            .filter_map(|x| x.name().ok())
            .position(|x| x == def_name)
    });

    let mut input_devs = get_input_devices(&host)?;
    drop(err_gag);

    let input_device = loop {
        if let Some(default_ix) = default_index {
            match CustomType::new(&format!("Select input device: [{default_ix}]"))
                .prompt_skippable()
            {
                Ok(Some(i)) => {
                    if let Some(device) = input_devs.nth(i) {
                        break device;
                    }
                }
                Ok(None) => {
                    if let Some(device) = input_devs.nth(default_ix) {
                        break device;
                    }
                }
                Err(InquireError::OperationInterrupted) => {
                    process::exit(0);
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            match inquire::prompt_usize("Select input device:") {
                Ok(i) => {
                    if let Some(device) = input_devs.nth(i) {
                        break device;
                    }
                }
                Err(InquireError::OperationInterrupted) => {
                    process::exit(0);
                }
                Err(e) => return Err(e.into()),
            }
        }
    };

    let time_limit = loop {
        match inquire::prompt_f32("Time limit (mins):") {
            Ok(m) if m > 0.0 => break Duration::from_secs_f32(m * 60.0),
            Ok(_) => {}
            Err(InquireError::OperationInterrupted) => {
                process::exit(0);
            }
            Err(e) => return Err(e.into()),
        }
    };

    let show_claps = {
        match inquire::prompt_confirmation("Show plap counts after each plap? (y/n):") {
            Ok(x) => x,
            Err(InquireError::OperationInterrupted) => {
                process::exit(0);
            }
            Err(e) => return Err(e.into()),
        }
    };

    let state = AppState::new(time_limit, show_claps);
    let state = Arc::new(Mutex::new(state));

    let config = input_device.default_input_config()?;

    // Run different processing based on sample format
    match config.sample_format() {
        cpal::SampleFormat::F32 => run::<f32>(input_device, config.into(), state)?,
        cpal::SampleFormat::I16 => run::<i16>(input_device, config.into(), state)?,
        cpal::SampleFormat::U16 => run::<u16>(input_device, config.into(), state)?,
        _ => return Err(anyhow::anyhow!("Unsupported sample format")),
    }

    Ok(())
}

fn run<T>(
    device: cpal::Device,
    config: cpal::StreamConfig,
    state: Arc<Mutex<AppState>>,
) -> anyhow::Result<()>
where
    T: SizedSample + FromSample<f32>,
    f32: cpal::FromSample<T>,
{
    let err_fn = |err| eprintln!("Error in audio stream: {}", err);

    let stream = device.build_input_stream(
        &config,
        {
            let state = Arc::clone(&state);
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                process_audio(data, &state);
            }
        },
        err_fn,
        None,
    )?;

    stream.play()?;

    loop {
        std::thread::sleep(Duration::from_secs_f32(1.0 / FREQUENCY));

        let mut state_lock = state.lock().unwrap();

        if !state_lock.is_active() {
            continue;
        }

        if !state_lock.calibrate() {
            continue;
        }

        let elapsed = Instant::now().duration_since(state_lock.timer_started);
        let Some(remaining) = state_lock.time_limit.checked_sub(elapsed) else {
            println!("Times up!");
            println!(
                "Hard plaps: {}        Soft plaps: {}",
                state_lock.hard_claps, state_lock.soft_claps
            );
            return Ok(());
        };

        let hard_threshold = state_lock.last_calibrate_max
            - (state_lock.last_calibrate_max - state_lock.baseline) * (1.0 - CALIBRATION_TOLERANCE);

        if state_lock.detect_peak() {
            let total_secs = remaining.as_secs();
            let mins = total_secs / 60;
            let secs = total_secs % 60;

            if state_lock.current_db >= hard_threshold {
                state_lock.hard_claps += 1;
                if state_lock.show_claps {
                    println!("Good girl~!           Hard plaps: {}      Soft plaps: {}      Time remaining: {:02}:{:02}", state_lock.hard_claps, state_lock.soft_claps, mins, secs);
                } else {
                    println!(
                        "Good girl~!           Time remaining: {:02}:{:02}",
                        mins, secs
                    );
                }
            } else {
                state_lock.soft_claps += 1;
                if state_lock.show_claps {
                    println!("Worthless paypig!     Hard plaps: {}      Soft plaps: {}      Time remaining: {:02}:{:02}", state_lock.hard_claps, state_lock.soft_claps, mins, secs);
                } else {
                    println!(
                        "Worthless paypig!     Time remaining: {:02}:{:02}",
                        mins, secs
                    );
                }
            }
        }
    }
}

fn process_audio<T>(data: &[T], state: &Arc<Mutex<AppState>>)
where
    T: Sample + FromSample<f32>,
    f32: cpal::FromSample<T>,
{
    let mut state_lock = state.lock().unwrap();

    // Calculate RMS (root mean square) of the audio buffer
    let sum_squares: f32 = data
        .iter()
        .map(|s| {
            let sample: f32 = s.to_sample();
            sample * sample
        })
        .sum();

    let rms = (sum_squares / data.len() as f32).sqrt();

    let db = if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f32::NEG_INFINITY
    };

    state_lock.current_db = db;

    if state_lock.baseline == 0.0 {
        state_lock.baseline = db;
    }

    if state_lock.baseline_samples < BASELINE_WINDOW {
        state_lock.baseline_samples += 1;
        state_lock.baseline_sum += db;
        state_lock.baseline = state_lock.baseline_sum / state_lock.baseline_samples as f32;
    } else {
        state_lock.baseline =
            (state_lock.baseline * (BASELINE_WINDOW - 1) as f32 + db) / BASELINE_WINDOW as f32;
    }
}

struct AppState {
    current_db: f32,
    baseline: f32,
    baseline_samples: usize,
    baseline_sum: f32,
    last_peak_distance: isize,
    started: Instant,
    last_calibrate_max: f32,
    calibration: CalibrationStatus,
    timer_started: Instant,
    time_limit: Duration,
    show_claps: bool,
    hard_claps: usize,
    soft_claps: usize,
}

enum CalibrationStatus {
    Waiting,
    Started { last_max_instant: Instant },
    Complete,
}

impl AppState {
    fn new(time_limit: Duration, show_claps: bool) -> Self {
        Self {
            current_db: 0.0,
            baseline: 0.0,
            baseline_samples: 0,
            baseline_sum: 0.0,
            last_peak_distance: -1,
            started: Instant::now(),
            last_calibrate_max: f32::NEG_INFINITY,
            calibration: CalibrationStatus::Waiting,
            timer_started: Instant::now(),
            time_limit,
            show_claps,
            hard_claps: 0,
            soft_claps: 0,
        }
    }

    fn is_active(&self) -> bool {
        Instant::now().duration_since(self.started) > ACTIVE_DELAY
    }

    fn calibrate(&mut self) -> bool {
        match self.calibration {
            CalibrationStatus::Waiting => {
                println!("Beginning calibration, plap HARD!");

                self.calibration = CalibrationStatus::Started {
                    last_max_instant: Instant::now(),
                };
                false
            }
            CalibrationStatus::Started { last_max_instant }
                if Instant::now().duration_since(last_max_instant) > CALIBRATION_COMPLETE_DELAY =>
            {
                println!("Calibration is complete, timer has started.");
                self.calibration = CalibrationStatus::Complete;
                self.timer_started = Instant::now();
                true
            }
            CalibrationStatus::Started {
                ref mut last_max_instant,
            } => {
                if self.current_db > self.last_calibrate_max {
                    self.last_calibrate_max = self.current_db;
                    *last_max_instant = Instant::now();
                }
                false
            }
            CalibrationStatus::Complete => true,
        }
    }

    fn peak_threshold(&self) -> f32 {
        self.baseline + PEAK_THRESHOLD
    }

    fn reset_threshold(&self) -> f32 {
        self.peak_threshold() - RESET_THRESHOLD
    }

    fn detect_peak(&mut self) -> bool {
        if self.last_peak_distance != -1 {
            self.reset_peak();
            return false;
        }

        if self.current_db > self.peak_threshold() {
            self.last_peak_distance = 0;
            true
        } else {
            false
        }
    }

    fn reset_peak(&mut self) {
        if self.last_peak_distance == -1 {
            return;
        }

        self.last_peak_distance += 1;
        if self.last_peak_distance > RESET_DISTANCE && self.current_db < self.reset_threshold() {
            self.last_peak_distance = -1;
        }
    }
}

fn get_input_devices<H: HostTrait>(host: &H) -> anyhow::Result<InputDevices<H::Devices>> {
    Ok(host.input_devices()?)
}
