//! Source live : mémoire partagée du SDK iRacing (Windows uniquement).
//!
//! iRacing publie `Local\IRSDKMemMapFileName` (header, YAML, table des variables,
//! jusqu'à 4 tampons rotatifs) et signale chaque nouvelle ligne via l'événement
//! `Local\IRSDKDataValidEvent`, à 60 Hz.
//!
//! La mémoire est modifiée par iRacing pendant qu'on la lit : on ne forme jamais de
//! `&[u8]` dessus, on copie avec `ptr::copy_nonoverlapping` puis on vérifie que le
//! `tickCount` du tampon n'a pas changé pendant la copie.

use std::time::{Duration, Instant};

use crate::irsdk::{HEADER_LEN, Header, VarTable, decode_session_yaml};
use crate::sample::ChannelMap;
use crate::session::SessionInfo;
use crate::source::{SourceError, SourceEvent, TelemetrySource};

#[cfg(windows)]
mod sys {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Memory::{
        FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
    };
    use windows::Win32::System::Threading::{
        OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
    };
    use windows::core::w;

    pub struct Mapping {
        map: HANDLE,
        event: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
    }

    // SAFETY : les handles Win32 et la vue sont utilisables depuis n'importe quel thread.
    unsafe impl Send for Mapping {}

    impl Mapping {
        pub fn open() -> Result<Self, String> {
            unsafe {
                let map =
                    OpenFileMappingW(FILE_MAP_READ.0, false, w!("Local\\IRSDKMemMapFileName"))
                        .map_err(|e| {
                            format!("mémoire partagée absente ({e}) — iRacing est-il lancé ?")
                        })?;
                let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0);
                if view.Value.is_null() {
                    let _ = CloseHandle(map);
                    return Err("MapViewOfFile a échoué".into());
                }
                let event = match OpenEventW(
                    SYNCHRONIZATION_SYNCHRONIZE,
                    false,
                    w!("Local\\IRSDKDataValidEvent"),
                ) {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = UnmapViewOfFile(view);
                        let _ = CloseHandle(map);
                        return Err(format!("événement de données absent ({e})"));
                    }
                };
                Ok(Mapping { map, event, view })
            }
        }

        /// Attend le signal « nouvelle ligne ». Retourne `false` sur délai dépassé.
        pub fn wait(&self, ms: u32) -> bool {
            unsafe { WaitForSingleObject(self.event, ms) == WAIT_OBJECT_0 }
        }

        /// Copie `out.len()` octets depuis le décalage `offset` de la vue.
        ///
        /// SAFETY : l'appelant garantit que `offset + out.len()` reste dans la vue
        /// (bornes déduites du header iRacing).
        pub unsafe fn copy(&self, offset: usize, out: &mut [u8]) {
            unsafe {
                let src = (self.view.Value as *const u8).add(offset);
                std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), out.len());
            }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                let _ = UnmapViewOfFile(self.view);
                let _ = CloseHandle(self.event);
                let _ = CloseHandle(self.map);
            }
        }
    }
}

#[cfg(not(windows))]
mod sys {
    pub struct Mapping;

    impl Mapping {
        pub fn open() -> Result<Self, String> {
            Err("la télémétrie live n'existe que sous Windows ; utiliser le mode rejeu".into())
        }
        pub fn wait(&self, _ms: u32) -> bool {
            false
        }
        /// # Safety
        /// Jamais appelée hors Windows.
        pub unsafe fn copy(&self, _offset: usize, _out: &mut [u8]) {}
    }
}

struct Connection {
    map: sys::Mapping,
    header: Header,
    channels: ChannelMap,
    session_info_update: i32,
    last_tick: i32,
    row: Vec<u8>,
}

pub struct LiveSource {
    conn: Option<Connection>,
    last_attempt: Option<Instant>,
    last_error: Option<String>,
    pending: Option<SourceEvent>,
}

impl Default for LiveSource {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveSource {
    pub fn new() -> Self {
        LiveSource {
            conn: None,
            last_attempt: None,
            last_error: None,
            pending: None,
        }
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn read_header(map: &sys::Mapping) -> Result<Header, SourceError> {
        let mut buf = [0u8; HEADER_LEN];
        // SAFETY : le header fait toujours 112 octets en tête de la vue.
        unsafe { map.copy(0, &mut buf) };
        Ok(Header::parse(&buf)?)
    }

    fn read_region(map: &sys::Mapping, offset: i32, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        // SAFETY : offset et len viennent du header iRacing, qui décrit la vue.
        unsafe { map.copy(offset.max(0) as usize, &mut v) };
        v
    }

    fn read_session_info(map: &sys::Mapping, h: &Header) -> SessionInfo {
        let raw = Self::read_region(
            map,
            h.session_info_offset,
            h.session_info_len.max(0) as usize,
        );
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        SessionInfo::parse(&decode_session_yaml(&raw[..end]))
    }

    fn try_connect(&mut self) -> Result<Option<SourceEvent>, SourceError> {
        let map = match sys::Mapping::open() {
            Ok(m) => m,
            Err(e) => {
                self.last_error = Some(e);
                return Ok(None);
            }
        };
        let header = Self::read_header(&map)?;
        if !header.is_connected() {
            self.last_error = Some("iRacing lancé mais aucune session active".into());
            return Ok(None);
        }
        // La table des variables est lue via une copie couvrant [0, fin des varHeaders).
        let vh_end = header.var_header_offset as usize
            + header.num_vars as usize * crate::irsdk::VAR_HEADER_LEN;
        let raw = Self::read_region(&map, 0, vh_end);
        let vars = VarTable::parse(&raw, &header)?;
        let missing = ChannelMap::missing_required(&vars);
        if !missing.is_empty() {
            return Err(SourceError::Unavailable(format!(
                "canaux obligatoires absents : {missing:?}"
            )));
        }
        let session = Self::read_session_info(&map, &header);
        self.last_error = None;
        self.conn = Some(Connection {
            channels: ChannelMap::new(&vars),
            session_info_update: header.session_info_update,
            last_tick: -1,
            row: vec![0u8; header.buf_len as usize],
            header,
            map,
        });
        Ok(Some(SourceEvent::Connected(session)))
    }
}

impl TelemetrySource for LiveSource {
    fn next_event(&mut self, timeout: Duration) -> Result<Option<SourceEvent>, SourceError> {
        if let Some(ev) = self.pending.take() {
            return Ok(Some(ev));
        }
        let Some(conn) = self.conn.as_mut() else {
            // Pas plus d'une tentative de connexion par seconde.
            if self
                .last_attempt
                .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
            {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            self.last_attempt = Some(Instant::now());
            return self.try_connect();
        };

        conn.map
            .wait(timeout.as_millis().min(u32::MAX as u128) as u32);
        let header = Self::read_header(&conn.map)?;
        if !header.is_connected() {
            self.conn = None;
            return Ok(Some(SourceEvent::Disconnected));
        }
        if header.num_vars != conn.header.num_vars || header.buf_len != conn.header.buf_len {
            // Structure changée (nouvelle session iRacing) : reconnexion complète.
            self.conn = None;
            self.last_attempt = None;
            return Ok(Some(SourceEvent::Disconnected));
        }

        let latest = header.latest_buf();
        if latest.tick_count == conn.last_tick {
            return Ok(None);
        }
        // Copie puis vérification que le tampon n'a pas été réécrit pendant la copie.
        for _ in 0..3 {
            // SAFETY : bufOffset + bufLen décrit un tampon de la vue.
            unsafe {
                conn.map
                    .copy(latest.buf_offset.max(0) as usize, &mut conn.row)
            };
            let after = Self::read_header(&conn.map)?;
            let still = after
                .var_buf
                .iter()
                .any(|vb| vb.buf_offset == latest.buf_offset && vb.tick_count == latest.tick_count);
            if still {
                conn.last_tick = latest.tick_count;
                let sample = conn.channels.decode(&conn.row);
                if header.session_info_update != conn.session_info_update {
                    conn.session_info_update = header.session_info_update;
                    let info = Self::read_session_info(&conn.map, &header);
                    self.pending = Some(SourceEvent::Sample(sample));
                    return Ok(Some(SourceEvent::SessionInfoChanged(info)));
                }
                return Ok(Some(SourceEvent::Sample(sample)));
            }
        }
        // Trois copies déchirées de suite : on saute ce tick.
        Ok(None)
    }

    fn describe(&self) -> String {
        "iRacing (live)".into()
    }
}
