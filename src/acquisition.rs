//! Módulo de aquisição de dados — hardware e REM window (durável via redb)

use crate::{
    types::*,
    rem::RemWindow,
    persistence::{PersistenceManager, ProtocolMeta},
    Result,
};
use std::collections::HashSet;
use tracing::{info, warn};

#[derive(Debug)]
pub struct Acquisition {
    config: Config,
    packets: Vec<DataPacket>,
    hardware_active: bool,
    experimenters: HashSet<Rater>,
    hardware_key: Rater,
    acquired_history: HashSet<PacketId>,
    next_id: PacketId,
    rem_window: RemWindow,
    persist: Option<PersistenceManager>,
}

#[derive(Debug, thiserror::Error)]
pub enum AcquisitionError {
    #[error("Hardware inactive")]
    HardwareInactive,
    #[error("Invalid timestamp: {0}")]
    InvalidTimestamp(String),
    #[error("Invalid sample length: {0}")]
    InvalidSampleLength(usize),
    #[error("Packet ID already used: {0}")]
    DuplicatePacketId(PacketId),
    #[error("Stimulus outside REM window")]
    StimulusOutsideRem,
    #[error("Packet limit reached: {0}")]
    PacketLimitReached(PacketId),
    #[error("REM window error: {0}")]
    RemWindow(#[from] crate::rem::RemWindowError),
    #[error("Persistence error: {0}")]
    Persistence(String),
}

impl Acquisition {
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            packets: Vec::new(),
            hardware_active: true,
            experimenters: config.experimenters.clone(),
            hardware_key: "hardware-key".to_string(),
            acquired_history: HashSet::new(),
            next_id: 0,
            rem_window: RemWindow::new(),
            persist: None,
        }
    }

    pub fn with_persistence(mut self, persist: PersistenceManager) -> Result<Self> {
        let meta = persist.load_meta().map_err(|e| {
            AcquisitionError::Persistence(format!("Failed to load meta: {}", e))
        })?;

        self.next_id = meta.next_packet_id;
        self.hardware_active = meta.hardware_active;
        self.rem_window = match (meta.rem_window_start, meta.rem_window_end) {
            (Some(s), Some(e)) => {
                let mut rw = RemWindow::new();
                rw.force_set(s, e);
                rw
            }
            _ => RemWindow::new(),
        };

        let stored = persist.acquisition.get_all().map_err(|e| {
            AcquisitionError::Persistence(format!("Failed to load packets: {}", e))
        })?;
        for (_, packet) in stored {
            self.acquired_history.insert(packet.id);
            self.packets.push(packet);
        }

        self.persist = Some(persist);
        info!(packets = self.packets.len(), next_id = self.next_id, "Acquisition recovered from disk");
        Ok(self)
    }

    pub fn acquire(
        &mut self,
        ts: Timestamp,
        sensor: SensorType,
        sample: &[Sample],
        now: Timestamp,
    ) -> Result<DataPacket> {
        if !self.hardware_active {
            return Err(AcquisitionError::HardwareInactive.into());
        }

        if ts < now.saturating_sub(self.config.max_clock_skew)
            || ts > now + self.config.max_clock_skew
        {
            return Err(AcquisitionError::InvalidTimestamp(
                format!("ts={} outside [{}, {}]", ts,
                    now.saturating_sub(self.config.max_clock_skew),
                    now + self.config.max_clock_skew)
            ).into());
        }

        if sample.is_empty() || sample.len() > self.config.max_sample_length {
            return Err(AcquisitionError::InvalidSampleLength(sample.len()).into());
        }

        if self.next_id >= self.config.max_packet_id {
            return Err(AcquisitionError::PacketLimitReached(self.next_id).into());
        }

        if sensor == SensorType::STIM && !self.rem_window.is_active(ts) {
            return Err(AcquisitionError::StimulusOutsideRem.into());
        }

        let packet = DataPacket {
            ts,
            sensor,
            sample: sample.to_vec(),
            id: self.next_id,
        };

        if let Some(ref persist) = self.persist {
            let key = format!("pkt:{}", packet.id);
            persist.acquisition.insert(&key, &packet).map_err(|e| {
                AcquisitionError::Persistence(format!("Failed to persist packet: {}", e))
            })?;
            persist.save_meta(&ProtocolMeta {
                next_packet_id: self.next_id + 1,
                next_epoch_id: 0,
                hardware_active: self.hardware_active,
                rem_window_start: self.rem_window.start(),
                rem_window_end: self.rem_window.end(),
                audit_chain_length: 0,
            }).map_err(|e| {
                AcquisitionError::Persistence(format!("Failed to persist meta: {}", e))
            })?;
        }

        self.packets.push(packet.clone());
        self.acquired_history.insert(packet.id);
        self.next_id += 1;

        info!(packet_id = packet.id, sensor = %sensor, ts = ts, "Packet acquired and persisted");
        Ok(packet)
    }

    pub fn update_rem_window(&mut self, start: Timestamp, end: Timestamp, now: Timestamp) -> Result<()> {
        self.rem_window.update(start, end, now, &self.config)?;

        if let Some(ref persist) = self.persist {
            persist.save_meta(&ProtocolMeta {
                next_packet_id: self.next_id,
                next_epoch_id: 0,
                hardware_active: self.hardware_active,
                rem_window_start: self.rem_window.start(),
                rem_window_end: self.rem_window.end(),
                audit_chain_length: 0,
            }).map_err(|e| {
                AcquisitionError::Persistence(format!("Failed to persist meta: {}", e))
            })?;
        }

        Ok(())
    }

    pub fn hardware_failure(&mut self) {
        self.hardware_active = false;
        warn!("Hardware failure");
        if let Some(ref persist) = self.persist {
            let _ = persist.save_meta(&ProtocolMeta {
                next_packet_id: self.next_id,
                next_epoch_id: 0,
                hardware_active: false,
                rem_window_start: self.rem_window.start(),
                rem_window_end: self.rem_window.end(),
                audit_chain_length: 0,
            });
        }
    }

    pub fn hardware_recovery(&mut self) {
        self.hardware_active = true;
        self.packets.clear();
        warn!("Hardware recovered");
        if let Some(ref persist) = self.persist {
            let _ = persist.save_meta(&ProtocolMeta {
                next_packet_id: self.next_id,
                next_epoch_id: 0,
                hardware_active: true,
                rem_window_start: self.rem_window.start(),
                rem_window_end: self.rem_window.end(),
                audit_chain_length: 0,
            });
        }
    }

    pub fn lose_packet(&mut self, index: usize) -> Option<DataPacket> {
        if index < self.packets.len() { Some(self.packets.remove(index)) } else { None }
    }

    pub fn packets(&self) -> &[DataPacket] { &self.packets }
    pub fn packets_in_range(&self, start: Timestamp, end: Timestamp) -> Vec<DataPacket> {
        self.packets.iter().filter(|p| p.ts >= start && p.ts <= end).cloned().collect()
    }
    pub fn has_packet(&self, id: PacketId) -> bool { self.acquired_history.contains(&id) }
    pub fn next_id(&self) -> PacketId { self.next_id }
    pub fn is_hardware_active(&self) -> bool { self.hardware_active }
    pub fn rem_window(&self) -> &RemWindow { &self.rem_window }
}