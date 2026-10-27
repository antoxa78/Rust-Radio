//! GStreamer playback (main thread only).
//!
//! One `playbin` per station. Events are reported to a single callback:
//! buffering progress, the moment audio actually starts, stream tags, real
//! spectrum magnitudes (via the `spectrum` element), and failure/end.

use gstreamer as gst;
use gstreamer::prelude::*;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Linear frequency bands requested from the `spectrum` element; they are
/// grouped into log-spaced bars by the UI.
pub const SPECTRUM_BANDS: u32 = 512;
/// Magnitudes below this (dB) are treated as silence.
pub const SPECTRUM_THRESHOLD_DB: i32 = -80;

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Buffering(u32),
    /// Audio is actually playing (first transition to PLAYING).
    Started,
    Tags {
        title: Option<String>,
        bitrate_bps: Option<u32>,
        codec: Option<String>,
    },
    /// dB magnitude per linear band, `SPECTRUM_BANDS` entries.
    Spectrum(Vec<f32>),
    Failed(String),
    Ended,
}

pub struct PlayOptions {
    pub sink: String,
    pub disable_video: bool,
    /// Pre-buffer before starting (adaptive to the bitrate) instead of
    /// starting as soon as possible.
    pub prebuffer: bool,
    pub bitrate_kbps: Option<u32>,
    pub volume: f64,
}

type EventCallback = Rc<dyn Fn(PlayerEvent)>;

#[derive(Default)]
struct Inner {
    pipeline: Option<gst::Element>,
    bus_guard: Option<gst::bus::BusWatchGuard>,
    safety_timeout: Option<glib::SourceId>,
}

pub struct Player {
    inner: RefCell<Inner>,
    generation: Cell<u64>,
    started: Cell<bool>,
    on_event: RefCell<Option<EventCallback>>,
}

/// Lower bitrate streams get more pre-buffering.
pub fn adaptive_buffer_ns(bitrate_kbps: Option<u32>) -> i64 {
    let seconds = match bitrate_kbps {
        Some(0..=96) => 20,
        Some(97..=192) => 15,
        Some(193..=320) => 10,
        Some(_) => 8,
        None => 10,
    };
    seconds * 1_000_000_000
}

/// Perceptual (cubic) volume, like `GstStreamVolume`'s cubic format.
pub fn linear_volume(slider: f64) -> f64 {
    slider.clamp(0.0, 1.0).powi(3)
}

impl Player {
    pub fn new() -> Rc<Self> {
        Rc::new(Player {
            inner: RefCell::new(Inner::default()),
            generation: Cell::new(0),
            started: Cell::new(false),
            on_event: RefCell::new(None),
        })
    }

    pub fn connect_event<F: Fn(PlayerEvent) + 'static>(&self, f: F) {
        *self.on_event.borrow_mut() = Some(Rc::new(f));
    }

    fn emit(&self, event: PlayerEvent) {
        let callback = self.on_event.borrow().clone();
        if let Some(callback) = callback {
            callback(event);
        }
    }

    pub fn stop(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.started.set(false);
        let mut inner = self.inner.borrow_mut();
        inner.bus_guard = None;
        if let Some(id) = inner.safety_timeout.take() {
            id.remove();
        }
        if let Some(pipeline) = inner.pipeline.take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
    }

    pub fn set_volume(&self, slider: f64) {
        if let Some(pipeline) = &self.inner.borrow().pipeline {
            pipeline.set_property("volume", linear_volume(slider));
        }
    }

    pub fn position(&self) -> Option<Duration> {
        let inner = self.inner.borrow();
        let position = inner
            .pipeline
            .as_ref()?
            .query_position::<gst::ClockTime>()?;
        Some(Duration::from_nanos(position.nseconds()))
    }

    /// (sample rate, bit depth) negotiated at the audio sink.
    pub fn audio_format(&self) -> (Option<i32>, Option<i32>) {
        let inner = self.inner.borrow();
        let Some(pipeline) = &inner.pipeline else {
            return (None, None);
        };
        let caps = pipeline
            .property::<Option<gst::Element>>("audio-sink")
            .and_then(|sink| sink.static_pad("sink"))
            .and_then(|pad| pad.current_caps());
        let Some(structure) = caps.as_ref().and_then(|c| c.structure(0)) else {
            return (None, None);
        };
        let rate = structure.get::<i32>("rate").ok();
        let depth = structure
            .get::<i32>("depth")
            .or_else(|_| structure.get::<i32>("width"))
            .ok()
            .or_else(|| {
                // audio/x-raw: derive from the format, e.g. S16LE / F32LE.
                let format = structure.get::<&str>("format").ok()?;
                format
                    .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                    .trim_end_matches(|c: char| c.is_ascii_alphabetic())
                    .parse()
                    .ok()
            });
        (rate, depth)
    }

    pub fn play(self: &Rc<Self>, url: &str, opts: &PlayOptions) -> Result<(), String> {
        self.stop();
        let generation = self.generation.get();

        let audio_sink = gst::ElementFactory::make(&opts.sink)
            .build()
            .or_else(|_| gst::ElementFactory::make("autoaudiosink").build())
            .map_err(|_| "no audio output available".to_string())?;
        let buffer_duration = if opts.prebuffer {
            adaptive_buffer_ns(opts.bitrate_kbps)
        } else {
            -1
        };
        let pipeline = gst::ElementFactory::make("playbin")
            .property("uri", url)
            .property("buffer-duration", buffer_duration)
            .property("audio-sink", &audio_sink)
            .build()
            .map_err(|_| "GStreamer playbin is not available".to_string())?;
        if opts.disable_video {
            if let Ok(fake) = gst::ElementFactory::make("fakesink").build() {
                pipeline.set_property("video-sink", &fake);
            }
        }
        // Real analyzer data. The element is in gst-plugins-good; if it's
        // missing the analyzer simply stays flat.
        if let Ok(spectrum) = gst::ElementFactory::make("spectrum")
            .property("bands", SPECTRUM_BANDS)
            .property("threshold", SPECTRUM_THRESHOLD_DB)
            .property("interval", 50_000_000u64)
            .property("post-messages", true)
            .build()
        {
            pipeline.set_property("audio-filter", &spectrum);
        }
        pipeline.set_property("volume", linear_volume(opts.volume));

        let bus = pipeline.bus().ok_or("pipeline has no bus")?;
        let weak: Weak<Player> = Rc::downgrade(self);
        let pipeline_weak = pipeline.downgrade();
        let prebuffer = opts.prebuffer;
        let guard = bus
            .add_watch_local(move |_, msg| {
                let (Some(player), Some(pipeline)) = (weak.upgrade(), pipeline_weak.upgrade())
                else {
                    return glib::ControlFlow::Break;
                };
                if player.generation.get() != generation {
                    return glib::ControlFlow::Break;
                }
                player.handle_message(msg, &pipeline, generation, prebuffer);
                glib::ControlFlow::Continue
            })
            .map_err(|err| err.to_string())?;

        let initial = if prebuffer {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        if let Err(err) = pipeline.set_state(initial) {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(format!("couldn't start the stream ({})", err));
        }

        // Some streams never post buffering messages; don't wait forever.
        let safety_timeout = prebuffer.then(|| {
            let weak = Rc::downgrade(self);
            let pipeline_weak = pipeline.downgrade();
            glib::timeout_add_local_once(Duration::from_secs(12), move || {
                let (Some(player), Some(pipeline)) = (weak.upgrade(), pipeline_weak.upgrade())
                else {
                    return;
                };
                player.inner.borrow_mut().safety_timeout = None;
                if player.generation.get() == generation && !player.started.get() {
                    let _ = pipeline.set_state(gst::State::Playing);
                }
            })
        });

        let mut inner = self.inner.borrow_mut();
        inner.pipeline = Some(pipeline);
        inner.bus_guard = Some(guard);
        inner.safety_timeout = safety_timeout;
        Ok(())
    }

    fn handle_message(
        self: &Rc<Self>,
        msg: &gst::Message,
        pipeline: &gst::Element,
        generation: u64,
        prebuffer: bool,
    ) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Buffering(b) => {
                let percent = b.percent().clamp(0, 100) as u32;
                if !prebuffer {
                    return;
                }
                self.emit(PlayerEvent::Buffering(percent));
                if percent == 100 {
                    let _ = pipeline.set_state(gst::State::Playing);
                }
            }
            MessageView::StateChanged(s) => {
                let from_pipeline = msg
                    .src()
                    .is_some_and(|src| src == pipeline.upcast_ref::<gst::Object>());
                if from_pipeline && s.current() == gst::State::Playing && !self.started.get() {
                    self.started.set(true);
                    if let Some(id) = self.inner.borrow_mut().safety_timeout.take() {
                        id.remove();
                    }
                    self.emit(PlayerEvent::Started);
                }
            }
            MessageView::Tag(t) => {
                let tags = t.tags();
                let title = tags
                    .index::<gst::tags::Title>(0)
                    .map(|v| v.get().to_string())
                    .or_else(|| {
                        tags.index::<gst::tags::Artist>(0)
                            .map(|v| v.get().to_string())
                    })
                    .filter(|s| !s.trim().is_empty());
                let bitrate_bps = tags
                    .index::<gst::tags::Bitrate>(0)
                    .or_else(|| tags.index::<gst::tags::NominalBitrate>(0))
                    .map(|v| v.get());
                let codec = tags
                    .index::<gst::tags::AudioCodec>(0)
                    .map(|v| v.get().to_string());
                if title.is_some() || bitrate_bps.is_some() || codec.is_some() {
                    self.emit(PlayerEvent::Tags {
                        title,
                        bitrate_bps,
                        codec,
                    });
                }
            }
            MessageView::Element(e) => {
                if let Some(s) = e.structure() {
                    if s.name() == "spectrum" {
                        if let Ok(list) = s.get::<gst::List>("magnitude") {
                            let mags: Vec<f32> =
                                list.iter().filter_map(|v| v.get::<f32>().ok()).collect();
                            self.emit(PlayerEvent::Spectrum(mags));
                        }
                    }
                }
            }
            MessageView::ClockLost(_) => {
                let _ = pipeline.set_state(gst::State::Paused);
                let _ = pipeline.set_state(gst::State::Playing);
            }
            MessageView::Error(err) => {
                let text = err.error().to_string();
                #[cfg(debug_assertions)]
                eprintln!("GStreamer error: {} ({:?})", text, err.debug());
                self.finish(generation, PlayerEvent::Failed(text));
            }
            MessageView::Eos(_) => self.finish(generation, PlayerEvent::Ended),
            _ => {}
        }
    }

    /// Tear down and report from an idle callback, so the handler may start
    /// a new stream without re-entering this bus watch.
    fn finish(self: &Rc<Self>, generation: u64, event: PlayerEvent) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(player) = weak.upgrade() {
                if player.generation.get() == generation {
                    player.stop();
                    player.emit(event);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_and_volume_curves() {
        assert_eq!(adaptive_buffer_ns(Some(64)), 20_000_000_000);
        assert_eq!(adaptive_buffer_ns(Some(320)), 10_000_000_000);
        assert_eq!(adaptive_buffer_ns(None), 10_000_000_000);
        assert_eq!(linear_volume(1.0), 1.0);
        assert!((linear_volume(0.5) - 0.125).abs() < 1e-9);
        assert_eq!(linear_volume(-1.0), 0.0);
    }
}
