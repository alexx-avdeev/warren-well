//! Config persistence in the `nvs` flash partition using `sequential-storage`.
//!
//! The default esp-idf partition table reserves `nvs` at 0x9000..0xF000
//! (24 KB = 6 sectors). The pure-Rust stack never uses NVS, so we own it.

use core::ops::Range;

use embassy_embedded_hal::adapter::BlockingAsync;
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;
use log::{info, warn};
use sequential_storage::cache::{Cache, Uncached};
use sequential_storage::map::{MapConfig, MapStorage, SerializationError, Value};
use tank_core::config::{CONFIG_VERSION, DeviceConfig};

const NVS_RANGE: Range<u32> = 0x9000..0xF000;
const KEY_CONFIG: u8 = 1;

type Flash = BlockingAsync<FlashStorage<'static>>;
type NoCache = Cache<Uncached, Uncached, Uncached, u8>;

/// Newtype so we can implement the storage `Value` trait (postcard encoding)
/// for the config without the core crate depending on the storage crate.
#[derive(Debug, Clone)]
struct Stored(DeviceConfig);

impl Value<'_> for Stored {
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, SerializationError> {
        postcard::to_slice(&self.0, buffer)
            .map(|s| s.len())
            .map_err(|_| SerializationError::BufferTooSmall)
    }

    fn deserialize_from(buffer: &[u8]) -> Result<(Self, usize), SerializationError> {
        postcard::from_bytes::<DeviceConfig>(buffer)
            .map(|c| (Stored(c), buffer.len()))
            .map_err(|_| SerializationError::InvalidFormat)
    }
}

pub struct ConfigStore {
    map: MapStorage<u8, Flash, NoCache>,
    buf: [u8; 768],
}

impl ConfigStore {
    pub fn new(flash: FLASH<'static>) -> Self {
        let storage = BlockingAsync::new(FlashStorage::new(flash));
        Self {
            map: MapStorage::new(storage, MapConfig::new(NVS_RANGE), Cache::new_uncached()),
            buf: [0; 768],
        }
    }

    pub async fn load(&mut self) -> Option<DeviceConfig> {
        match self
            .map
            .fetch_item::<Stored>(&mut self.buf, &KEY_CONFIG)
            .await
        {
            Ok(Some(Stored(cfg))) if cfg.version == CONFIG_VERSION => {
                info!("config loaded from flash");
                Some(cfg)
            }
            Ok(Some(Stored(cfg))) => {
                warn!(
                    "config version {} != {}, using defaults",
                    cfg.version, CONFIG_VERSION
                );
                None
            }
            Ok(None) => {
                info!("no config in flash, using defaults");
                None
            }
            Err(e) => {
                warn!("config load failed: {e:?}, using defaults");
                None
            }
        }
    }

    pub async fn save(&mut self, cfg: &DeviceConfig) -> bool {
        let stored = Stored(cfg.clone());
        match self
            .map
            .store_item(&mut self.buf, &KEY_CONFIG, &stored)
            .await
        {
            Ok(()) => {
                info!("config saved");
                true
            }
            Err(e) => {
                warn!("config save failed: {e:?}");
                false
            }
        }
    }
}
