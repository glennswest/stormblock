//! SCSI ALUA (Asymmetric Logical Unit Access) for multipath I/O.
//!
//! Implements Target Port Group (TPG) state management and the
//! REPORT_TARGET_PORT_GROUPS / SET_TARGET_PORT_GROUPS SCSI commands.
//! Reference: SPC-4 §5.16, §6.35, §6.36

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use serde::Serialize;

/// SCSI opcode for MAINTENANCE_IN (REPORT_TARGET_PORT_GROUPS uses service action 0x0A).
pub const MAINTENANCE_IN: u8 = 0xA3;
/// SCSI opcode for MAINTENANCE_OUT (SET_TARGET_PORT_GROUPS uses service action 0x0A).
pub const MAINTENANCE_OUT: u8 = 0xA4;
/// Service action for REPORT_TARGET_PORT_GROUPS.
pub const SA_REPORT_TPG: u8 = 0x0A;
/// Service action for SET_TARGET_PORT_GROUPS.
pub const SA_SET_TPG: u8 = 0x0A;

/// ALUA access state for a target port group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[repr(u8)]
pub enum AluaState {
    ActiveOptimized = 0x00,
    ActiveNonOptimized = 0x01,
    Standby = 0x02,
    Unavailable = 0x03,
    Transitioning = 0x0F,
}

impl AluaState {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b & 0x0F {
            0x00 => Some(AluaState::ActiveOptimized),
            0x01 => Some(AluaState::ActiveNonOptimized),
            0x02 => Some(AluaState::Standby),
            0x03 => Some(AluaState::Unavailable),
            0x0F => Some(AluaState::Transitioning),
            _ => None,
        }
    }
}

/// A target port group descriptor.
pub struct TargetPortGroup {
    /// Target port group ID (TPGID).
    pub tpg_id: u16,
    /// Current ALUA access state.
    state: AtomicU8,
    /// Supported ALUA states bitmask.
    pub supported_states: u8,
    /// Relative target port identifiers in this group.
    pub port_ids: Vec<u16>,
}

impl TargetPortGroup {
    pub fn new(tpg_id: u16, initial_state: AluaState, port_ids: Vec<u16>) -> Self {
        // Support AO, ANO, Standby, Unavailable
        let supported = 0x01 | 0x02 | 0x04 | 0x08;
        TargetPortGroup {
            tpg_id,
            state: AtomicU8::new(initial_state as u8),
            supported_states: supported,
            port_ids,
        }
    }

    pub fn state(&self) -> AluaState {
        AluaState::from_byte(self.state.load(Ordering::Relaxed))
            .unwrap_or(AluaState::ActiveOptimized)
    }

    pub fn set_state(&self, new_state: AluaState) {
        self.state.store(new_state as u8, Ordering::Relaxed);
    }
}

/// ALUA controller managing target port groups.
pub struct AluaController {
    pub groups: Vec<Arc<TargetPortGroup>>,
}

impl AluaController {
    /// Create a default single-group ALUA controller (active/optimized).
    pub fn new_single(port_ids: Vec<u16>) -> Self {
        let group = Arc::new(TargetPortGroup::new(
            1, // TPG ID 1
            AluaState::ActiveOptimized,
            port_ids,
        ));
        AluaController { groups: vec![group] }
    }

    /// Build REPORT TARGET PORT GROUPS response data (SPC-4 §6.35).
    pub fn report_target_port_groups(&self) -> Vec<u8> {
        let mut data = Vec::new();

        // Reserve 4 bytes for return data length (filled at end)
        data.extend_from_slice(&[0u8; 4]);

        for group in &self.groups {
            // 8-byte TPG descriptor
            let state = group.state();
            // Byte 0: ALUA access state (bits 3:0), Preferred (bit 7)
            let byte0 = (state as u8) | 0x80; // set preferred bit for active/optimized
            data.push(byte0);
            // Byte 1: supported states
            data.push(group.supported_states);
            // Bytes 2-3: TPG ID
            data.extend_from_slice(&group.tpg_id.to_be_bytes());
            // Byte 4: reserved
            data.push(0);
            // Byte 5: status code (0 = no status available)
            data.push(0);
            // Byte 6: vendor specific
            data.push(0);
            // Byte 7: target port count
            data.push(group.port_ids.len() as u8);

            // 4-byte target port descriptors
            for &port_id in &group.port_ids {
                data.extend_from_slice(&[0u8; 2]); // obsolete
                data.extend_from_slice(&port_id.to_be_bytes());
            }
        }

        // Fill in return data length (total - 4 header bytes)
        let data_len = (data.len() - 4) as u32;
        data[0..4].copy_from_slice(&data_len.to_be_bytes());

        data
    }

    /// Process SET TARGET PORT GROUPS command (SPC-4 §6.36).
    pub fn set_target_port_groups(&self, param_data: &[u8]) -> bool {
        // Parameter data contains 4-byte descriptors: [state, reserved, tpg_id_hi, tpg_id_lo]
        let mut offset = 0;
        while offset + 4 <= param_data.len() {
            let requested_state = param_data[offset] & 0x0F;
            let tpg_id = u16::from_be_bytes([param_data[offset + 2], param_data[offset + 3]]);

            if let Some(new_state) = AluaState::from_byte(requested_state) {
                if let Some(group) = self.groups.iter().find(|g| g.tpg_id == tpg_id) {
                    group.set_state(new_state);
                    tracing::info!("ALUA: TPG {tpg_id} set to {new_state:?}");
                }
            }
            offset += 4;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alua_single_group() {
        let ctrl = AluaController::new_single(vec![1, 2]);
        assert_eq!(ctrl.groups.len(), 1);
        assert_eq!(ctrl.groups[0].state(), AluaState::ActiveOptimized);
        assert_eq!(ctrl.groups[0].port_ids, vec![1, 2]);
    }

    #[test]
    fn report_target_port_groups() {
        let ctrl = AluaController::new_single(vec![1]);
        let data = ctrl.report_target_port_groups();
        // 4 header + 8 TPG descriptor + 4 port descriptor = 16
        assert_eq!(data.len(), 16);
        let data_len = u32::from_be_bytes(data[0..4].try_into().unwrap());
        assert_eq!(data_len, 12);
    }

    #[test]
    fn set_target_port_groups() {
        let ctrl = AluaController::new_single(vec![1]);
        // Set TPG 1 to standby
        let param = [AluaState::Standby as u8, 0, 0, 1];
        assert!(ctrl.set_target_port_groups(&param));
        assert_eq!(ctrl.groups[0].state(), AluaState::Standby);
    }
}
