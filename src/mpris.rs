//! MPRIS2 D-Bus interface, so media keys, the GNOME/KDE media controls and
//! tools like `playerctl` can control Rust Radio. Implemented with GIO's
//! D-Bus support (main thread, no extra dependencies).

use gtk::gio;
use gtk::glib;
use gtk::glib::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

const BUS_NAME: &str = "org.mpris.MediaPlayer2.rustradio";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

const INTROSPECTION: &str = r#"
<node>
  <interface name="org.mpris.MediaPlayer2">
    <method name="Raise"/>
    <method name="Quit"/>
    <property name="CanQuit" type="b" access="read"/>
    <property name="CanRaise" type="b" access="read"/>
    <property name="HasTrackList" type="b" access="read"/>
    <property name="Identity" type="s" access="read"/>
    <property name="DesktopEntry" type="s" access="read"/>
    <property name="SupportedUriSchemes" type="as" access="read"/>
    <property name="SupportedMimeTypes" type="as" access="read"/>
  </interface>
  <interface name="org.mpris.MediaPlayer2.Player">
    <method name="Next"/>
    <method name="Previous"/>
    <method name="Pause"/>
    <method name="PlayPause"/>
    <method name="Stop"/>
    <method name="Play"/>
    <method name="Seek"><arg direction="in" name="Offset" type="x"/></method>
    <method name="SetPosition">
      <arg direction="in" name="TrackId" type="o"/>
      <arg direction="in" name="Position" type="x"/>
    </method>
    <method name="OpenUri"><arg direction="in" name="Uri" type="s"/></method>
    <signal name="Seeked"><arg name="Position" type="x"/></signal>
    <property name="PlaybackStatus" type="s" access="read"/>
    <property name="Rate" type="d" access="readwrite"/>
    <property name="Metadata" type="a{sv}" access="read"/>
    <property name="Volume" type="d" access="readwrite"/>
    <property name="Position" type="x" access="read"/>
    <property name="MinimumRate" type="d" access="read"/>
    <property name="MaximumRate" type="d" access="read"/>
    <property name="CanGoNext" type="b" access="read"/>
    <property name="CanGoPrevious" type="b" access="read"/>
    <property name="CanPlay" type="b" access="read"/>
    <property name="CanPause" type="b" access="read"/>
    <property name="CanSeek" type="b" access="read"/>
    <property name="CanControl" type="b" access="read"/>
  </interface>
</node>
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MprisCommand {
    Raise,
    Quit,
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MprisState {
    /// "Playing", "Paused" or "Stopped".
    pub status: &'static str,
    pub station: Option<String>,
    pub title: Option<String>,
    pub volume: f64,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_play: bool,
    pub track_number: u64,
}

impl Default for MprisState {
    fn default() -> Self {
        MprisState {
            status: "Stopped",
            station: None,
            title: None,
            volume: 1.0,
            can_next: false,
            can_previous: false,
            can_play: false,
            track_number: 0,
        }
    }
}

type CommandFn = Rc<dyn Fn(MprisCommand)>;
type VolumeFn = Rc<dyn Fn(f64)>;

pub struct Mpris {
    connection: RefCell<Option<gio::DBusConnection>>,
    state: RefCell<MprisState>,
    on_command: CommandFn,
    on_volume: VolumeFn,
    _owner: RefCell<Option<gio::OwnerId>>,
}

impl Mpris {
    pub fn start<C, V>(on_command: C, on_volume: V) -> Rc<Self>
    where
        C: Fn(MprisCommand) + 'static,
        V: Fn(f64) + 'static,
    {
        let mpris = Rc::new(Mpris {
            connection: RefCell::new(None),
            state: RefCell::new(MprisState::default()),
            on_command: Rc::new(on_command),
            on_volume: Rc::new(on_volume),
            _owner: RefCell::new(None),
        });
        let weak = Rc::downgrade(&mpris);
        let owner = gio::bus_own_name(
            gio::BusType::Session,
            BUS_NAME,
            gio::BusNameOwnerFlags::NONE,
            move |connection, _| {
                if let Some(mpris) = weak.upgrade() {
                    if let Err(err) = mpris.register(&connection) {
                        eprintln!("MPRIS: failed to register: {}", err);
                    } else {
                        *mpris.connection.borrow_mut() = Some(connection);
                    }
                }
            },
            |_, _| {},
            |_, _| {},
        );
        *mpris._owner.borrow_mut() = Some(owner);
        mpris
    }

    fn register(self: &Rc<Self>, connection: &gio::DBusConnection) -> Result<(), glib::Error> {
        let node = gio::DBusNodeInfo::for_xml(INTROSPECTION)?;
        for iface_name in [ROOT_IFACE, PLAYER_IFACE] {
            let iface = node.lookup_interface(iface_name).expect("interface in XML");
            let for_call = Rc::downgrade(self);
            let for_get = Rc::downgrade(self);
            let for_set = Rc::downgrade(self);
            connection
                .register_object(OBJECT_PATH, &iface)
                .method_call(move |_, _, _, _, method, _params, invocation| {
                    let command = match method {
                        "Raise" => Some(MprisCommand::Raise),
                        "Quit" => Some(MprisCommand::Quit),
                        "Play" => Some(MprisCommand::Play),
                        "Pause" => Some(MprisCommand::Pause),
                        "PlayPause" => Some(MprisCommand::PlayPause),
                        "Stop" => Some(MprisCommand::Stop),
                        "Next" => Some(MprisCommand::Next),
                        "Previous" => Some(MprisCommand::Previous),
                        _ => None, // Seek / SetPosition / OpenUri: live radio can't seek.
                    };
                    invocation.return_value(None);
                    if let (Some(command), Some(mpris)) = (command, for_call.upgrade()) {
                        // Run after the reply so a slow handler can't time out the caller.
                        let callback = mpris.on_command.clone();
                        glib::idle_add_local_once(move || callback(command));
                    }
                })
                .property(move |_, _, _, iface, property| {
                    for_get
                        .upgrade()
                        .map(|mpris| mpris.property_value(iface, property))
                        .unwrap_or_else(|| false.to_variant())
                })
                .set_property(move |_, _, _, _, property, value| {
                    if property == "Volume" {
                        if let (Some(volume), Some(mpris)) = (value.get::<f64>(), for_set.upgrade())
                        {
                            (mpris.on_volume)(volume.clamp(0.0, 1.0));
                        }
                    }
                    true
                })
                .build()?;
        }
        Ok(())
    }

    fn metadata(state: &MprisState) -> glib::Variant {
        let dict = glib::VariantDict::new(None);
        let track_path = format!("/io/github/antoxa78/RustRadio/track/{}", state.track_number);
        if let Ok(path) = glib::variant::ObjectPath::try_from(track_path) {
            dict.insert_value("mpris:trackid", &path.to_variant());
        }
        if let Some(station) = &state.station {
            let title = state.title.clone().unwrap_or_else(|| station.clone());
            dict.insert_value("xesam:title", &title.to_variant());
            dict.insert_value("xesam:artist", &vec![station.clone()].to_variant());
            dict.insert_value("xesam:album", &station.to_variant());
        }
        dict.end()
    }

    fn property_value(&self, iface: &str, property: &str) -> glib::Variant {
        let s = self.state.borrow();
        match (iface, property) {
            (ROOT_IFACE, "CanQuit") => true.to_variant(),
            (ROOT_IFACE, "CanRaise") => true.to_variant(),
            (ROOT_IFACE, "HasTrackList") => false.to_variant(),
            (ROOT_IFACE, "Identity") => "Rust Radio".to_variant(),
            (ROOT_IFACE, "DesktopEntry") => "rust-radio".to_variant(),
            (ROOT_IFACE, "SupportedUriSchemes") => vec!["http", "https"].to_variant(),
            (ROOT_IFACE, "SupportedMimeTypes") => Vec::<String>::new().to_variant(),
            (_, "PlaybackStatus") => s.status.to_variant(),
            (_, "Rate" | "MinimumRate" | "MaximumRate") => 1.0f64.to_variant(),
            (_, "Metadata") => Self::metadata(&s),
            (_, "Volume") => s.volume.to_variant(),
            (_, "Position") => 0i64.to_variant(),
            (_, "CanGoNext") => s.can_next.to_variant(),
            (_, "CanGoPrevious") => s.can_previous.to_variant(),
            (_, "CanPlay") => s.can_play.to_variant(),
            (_, "CanPause") => (s.status == "Playing").to_variant(),
            (_, "CanSeek") => false.to_variant(),
            (_, "CanControl") => true.to_variant(),
            _ => false.to_variant(),
        }
    }

    /// Update the exported state and notify listeners of what changed.
    pub fn update(&self, f: impl FnOnce(&mut MprisState)) {
        let old = self.state.borrow().clone();
        let mut new = old.clone();
        f(&mut new);
        if new == old {
            return;
        }
        *self.state.borrow_mut() = new.clone();
        let Some(connection) = self.connection.borrow().clone() else {
            return;
        };
        let mut changed: Vec<(&str, glib::Variant)> = Vec::new();
        if new.status != old.status {
            changed.push(("PlaybackStatus", new.status.to_variant()));
            changed.push(("CanPause", (new.status == "Playing").to_variant()));
        }
        if new.station != old.station
            || new.title != old.title
            || new.track_number != old.track_number
        {
            changed.push(("Metadata", Self::metadata(&new)));
        }
        if new.volume != old.volume {
            changed.push(("Volume", new.volume.to_variant()));
        }
        if new.can_next != old.can_next {
            changed.push(("CanGoNext", new.can_next.to_variant()));
        }
        if new.can_previous != old.can_previous {
            changed.push(("CanGoPrevious", new.can_previous.to_variant()));
        }
        if new.can_play != old.can_play {
            changed.push(("CanPlay", new.can_play.to_variant()));
        }
        let dict = glib::VariantDict::new(None);
        for (name, value) in &changed {
            dict.insert_value(name, value);
        }
        let params = glib::Variant::tuple_from_iter([
            PLAYER_IFACE.to_variant(),
            dict.end(),
            Vec::<String>::new().to_variant(),
        ]);
        let _ = connection.emit_signal(
            None,
            OBJECT_PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            Some(&params),
        );
    }
}
