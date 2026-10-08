//! Inline audio playback.
//!
//! Why a private `playbin` instead of [`gtk::MediaFile`]? GTK's media backend is
//! built on GstPlay, which hardcodes the `playbin3` pipeline and attaches its GL
//! video sink. On some systems that combination races `decodebin3`'s handling of
//! `id3demux` for mp3 files carrying an ID3v2 tag and aborts the whole process
//! (`gstdecodebin3.c: mq_slot_handle_stream_start: assertion failed:
//! (collection)`), deterministically and even without pressing play. `playbin`
//! (v1) uses `decodebin`, which handles the same files fine, so audio previews
//! go through it instead. Video previews keep using `GtkMediaFile`.
//!
//! The [`gstreamer`](https://crates.io/crates/gstreamer) binding is optional
//! (the `audio` Cargo feature, on by default). With it disabled, [`AudioPlayer`]
//! is a stub whose [`AudioPlayer::new`] always returns `None`, and no audio file
//! is offered an inline player.

#[cfg(feature = "audio")]
mod backend {
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::Once;

    use gstreamer as gst;
    use gstreamer::prelude::*;
    use relm4::gtk::gio::prelude::FileExt;
    use relm4::gtk::{gio, glib};

    static INIT: Once = Once::new();

    /// One audio stream playing through a private `playbin` element.
    pub struct AudioPlayer {
        pipeline: gst::Element,
        /// Kept alive (and dropped) to keep the bus watch installed.
        _bus_watch: RefCell<Option<gst::bus::BusWatchGuard>>,
    }

    impl AudioPlayer {
        /// Build a paused-ready player for `path`, or `None` if `playbin` is
        /// unavailable. The pipeline stays un-prerolled until playback starts,
        /// so a directory with many audio files does not spin up a sink each.
        pub fn new(path: &Path) -> Option<Rc<Self>> {
            INIT.call_once(|| {
                let _ = gst::init();
            });

            let pipeline = match gst::ElementFactory::make("playbin").build() {
                Ok(pipeline) => pipeline,
                Err(err) => {
                    eprintln!("tree-space: playbin unavailable: {err}");
                    return None;
                }
            };
            pipeline.set_property("uri", gio::File::for_path(path).uri().as_str());
            // Match the video path: audible, with the panel's volume control.
            pipeline.set_property("mute", false);
            pipeline.set_property("volume", 1.0);

            let player = Rc::new(AudioPlayer {
                pipeline,
                _bus_watch: RefCell::new(None),
            });
            player.install_bus_watch();
            Some(player)
        }

        /// Watch the bus for end-of-stream and errors. On EOS the stream is
        /// rewound and paused so pressing play again starts from the top.
        fn install_bus_watch(self: &Rc<Self>) {
            let Some(bus) = self.pipeline.bus() else {
                return;
            };
            let weak = Rc::downgrade(self);
            let watch = bus.add_watch_local(move |_, message| {
                use gst::MessageView;
                let Some(player) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                match message.view() {
                    MessageView::Eos(..) => {
                        let _ = player.pipeline.set_state(gst::State::Paused);
                        let _ = player.pipeline.seek_simple(
                            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                            gst::ClockTime::ZERO,
                        );
                    }
                    MessageView::Error(err) => {
                        eprintln!("tree-space: audio playback error: {}", err.error());
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            });
            if let Ok(guard) = watch {
                *self._bus_watch.borrow_mut() = Some(guard);
            }
        }

        pub fn is_playing(&self) -> bool {
            self.pipeline.current_state() == gst::State::Playing
        }

        pub fn set_playing(&self, playing: bool) {
            let state = if playing {
                gst::State::Playing
            } else {
                gst::State::Paused
            };
            let _ = self.pipeline.set_state(state);
        }

        /// Current position in microseconds (0 while unknown).
        pub fn position(&self) -> i64 {
            self.pipeline
                .query_position::<gst::ClockTime>()
                .map(|t| t.useconds() as i64)
                .unwrap_or(0)
        }

        /// Stream duration in microseconds (0 while unknown).
        pub fn duration(&self) -> i64 {
            self.pipeline
                .query_duration::<gst::ClockTime>()
                .map(|t| t.useconds() as i64)
                .unwrap_or(0)
        }

        /// Seek to `microseconds` from the start.
        pub fn seek(&self, microseconds: i64) {
            if microseconds <= 0 {
                return;
            }
            let _ = self.pipeline.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_useconds(microseconds as u64),
            );
        }

        pub fn set_volume(&self, volume: f64) {
            self.pipeline.set_property("volume", volume.clamp(0.0, 1.0));
        }

        pub fn volume(&self) -> f64 {
            self.pipeline.property::<f64>("volume")
        }
    }

    impl Drop for AudioPlayer {
        fn drop(&mut self) {
            // Return the pipeline to NULL so its sink is released promptly.
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }
}

#[cfg(not(feature = "audio"))]
mod backend {
    use std::path::Path;
    use std::rc::Rc;

    /// Stub used when the `audio` feature is disabled: audio files are not
    /// offered an inline player.
    pub struct AudioPlayer;

    impl AudioPlayer {
        pub fn new(_path: &Path) -> Option<Rc<Self>> {
            None
        }

        pub fn is_playing(&self) -> bool {
            false
        }

        pub fn set_playing(&self, _playing: bool) {}

        pub fn position(&self) -> i64 {
            0
        }

        pub fn duration(&self) -> i64 {
            0
        }

        pub fn seek(&self, _microseconds: i64) {}

        pub fn set_volume(&self, _volume: f64) {}

        pub fn volume(&self) -> f64 {
            1.0
        }
    }
}

pub use backend::AudioPlayer;
