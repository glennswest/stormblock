//! iSCSI login state machine — security negotiation, CHAP, operational parameters.
//!
//! Reference: RFC 7143 §6, §11.12-13

use super::chap::{ChapAuthenticator, ChapConfig};
use super::pdu::{Bhs, IscsiPdu, Opcode, encode_text_params, parse_text_params};
use super::session::SessionParams;

/// Login phase stages (CSG/NSG values).
pub const STAGE_SECURITY: u8 = 0;
pub const STAGE_OPERATIONAL: u8 = 1;
pub const STAGE_FULL_FEATURE: u8 = 3;

/// Login response status classes and details.
#[derive(Debug, Clone, Copy)]
pub enum LoginStatus {
    Success,
    TargetMovedTemp,
    TargetMovedPerm,
    InitiatorError,
    AuthFailure,
    TargetNotFound,
    TargetError,
}

impl LoginStatus {
    pub fn class_detail(&self) -> (u8, u8) {
        match self {
            LoginStatus::Success => (0, 0),
            LoginStatus::TargetMovedTemp => (1, 1),
            LoginStatus::TargetMovedPerm => (1, 2),
            LoginStatus::InitiatorError => (2, 0),
            LoginStatus::AuthFailure => (2, 1),
            LoginStatus::TargetNotFound => (2, 2),
            LoginStatus::TargetError => (3, 0),
        }
    }
}

/// Login state machine result.
pub enum LoginResult {
    /// Send this response PDU and continue login.
    Continue(IscsiPdu),
    /// Login complete — final response PDU + negotiated params.
    Complete(IscsiPdu, SessionParams),
    /// Login failed — send this response and disconnect.
    Failed(IscsiPdu),
}

/// Login state machine processing login request PDUs.
pub struct LoginStateMachine {
    target_name: String,
    chap_config: Option<ChapConfig>,
    chap_auth: Option<ChapAuthenticator>,
    params: SessionParams,
    stage: u8,
    auth_complete: bool,
    security_offered: bool,
    /// StatSN for the next login response (initiator's ExpStatSN follows it).
    stat_sn: u32,
    /// ExpCmdSN derived from the latest login request's CmdSN.
    exp_cmd_sn: u32,
}

impl LoginStateMachine {
    pub fn new(target_name: String, chap_config: Option<ChapConfig>) -> Self {
        LoginStateMachine {
            target_name,
            chap_config,
            chap_auth: None,
            params: SessionParams::default(),
            stage: STAGE_SECURITY,
            auth_complete: false,
            security_offered: false,
            stat_sn: 1,
            exp_cmd_sn: 1,
        }
    }

    /// StatSN the full-feature phase should use for its first response.
    pub fn next_stat_sn(&self) -> u32 {
        self.stat_sn
    }

    /// ExpCmdSN after the final login request — the CmdSN the initiator
    /// will use for its first full-feature-phase command.
    pub fn exp_cmd_sn(&self) -> u32 {
        self.exp_cmd_sn
    }

    /// Process a login request PDU and return the response action.
    pub fn process(&mut self, req: &IscsiPdu) -> LoginResult {
        let req_csg = req.bhs.csg();
        let req_nsg = req.bhs.nsg();
        let transit = req.bhs.transit();
        let text_params = parse_text_params(&req.data);

        // Login PDUs carry the session's initial CmdSN. Immediate PDUs do not
        // advance CmdSN (RFC 7143 §4.2.2.1), so the first full-feature command
        // reuses the login CmdSN when the I bit was set.
        self.exp_cmd_sn = req
            .bhs
            .cmd_sn()
            .wrapping_add(if req.bhs.is_immediate() { 0 } else { 1 });

        match req_csg {
            STAGE_SECURITY => self.handle_security(&text_params, req, transit, req_nsg),
            STAGE_OPERATIONAL => self.handle_operational(&text_params, req, transit, req_nsg),
            _ => self.make_error_response(req, LoginStatus::InitiatorError),
        }
    }

    fn handle_security(
        &mut self,
        params: &[(String, String)],
        req: &IscsiPdu,
        transit: bool,
        nsg: u8,
    ) -> LoginResult {
        let mut response_params: Vec<(&str, &str)> = Vec::new();
        let mut chap_response_params: Vec<(String, String)> = Vec::new();

        for (key, val) in params {
            match key.as_str() {
                "InitiatorName" => {
                    self.params.initiator_name = val.clone();
                }
                "TargetName" => {
                    if val != &self.target_name {
                        tracing::warn!("Login: unknown target '{val}'");
                        return self.make_error_response(req, LoginStatus::TargetNotFound);
                    }
                    self.params.target_name = val.clone();
                    response_params.push(("TargetAlias", "StormBlock"));
                }
                "SessionType" => {
                    // Declarative (RFC 7143 §12.21): initiator-to-target only.
                    // Echoing it back makes open-iscsi abort the login with
                    // "couldn't recognize text SessionType=Discovery", which
                    // broke SendTargets discovery entirely.
                    self.params.discovery_session = val == "Discovery";
                }
                "AuthMethod" => {
                    if self.chap_config.is_some() && val.contains("CHAP") {
                        response_params.push(("AuthMethod", "CHAP"));
                    } else if val.contains("None") || self.chap_config.is_none() {
                        response_params.push(("AuthMethod", "None"));
                        self.auth_complete = true;
                    } else {
                        return self.make_error_response(req, LoginStatus::AuthFailure);
                    }
                }
                "CHAP_A" => {
                    // Initiator proposes algorithm — we only support MD5 (5)
                    if val.contains('5') {
                        if let Some(ref config) = self.chap_config {
                            let auth = ChapAuthenticator::new(config.clone());
                            for (k, v) in auth.challenge_params() {
                                chap_response_params.push((k, v));
                            }
                            self.chap_auth = Some(auth);
                            self.security_offered = true;
                        }
                    } else {
                        return self.make_error_response(req, LoginStatus::AuthFailure);
                    }
                }
                "CHAP_N" | "CHAP_R" => {
                    // Will be processed below as a pair
                }
                _ => {}
            }
        }

        // Check for CHAP response verification
        let chap_n = params.iter().find(|(k, _)| k == "CHAP_N").map(|(_, v)| v.as_str());
        let chap_r = params.iter().find(|(k, _)| k == "CHAP_R").map(|(_, v)| v.as_str());

        if let (Some(name), Some(response)) = (chap_n, chap_r) {
            if let Some(ref auth) = self.chap_auth {
                if auth.verify(name, response) {
                    self.auth_complete = true;
                    tracing::info!("CHAP authentication successful for '{name}'");
                } else {
                    tracing::warn!("CHAP authentication failed for '{name}'");
                    return self.make_error_response(req, LoginStatus::AuthFailure);
                }
            }
        }

        // Build response text data
        let mut all_params: Vec<(&str, &str)> = response_params;
        let owned_refs: Vec<(&str, &str)> = chap_response_params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        all_params.extend(owned_refs);

        let resp_data = encode_text_params(&all_params);

        if transit && self.auth_complete {
            if nsg == STAGE_FULL_FEATURE {
                // Skip operational negotiation — go straight to full feature
                self.stage = STAGE_FULL_FEATURE;
                let pdu = self.make_login_response(req, &resp_data, true, STAGE_SECURITY, STAGE_FULL_FEATURE, LoginStatus::Success);
                LoginResult::Complete(pdu, self.params.clone())
            } else {
                // Transition to operational negotiation
                self.stage = STAGE_OPERATIONAL;
                let pdu = self.make_login_response(req, &resp_data, true, STAGE_SECURITY, STAGE_OPERATIONAL, LoginStatus::Success);
                LoginResult::Continue(pdu)
            }
        } else {
            // Stay in security stage
            let pdu = self.make_login_response(req, &resp_data, false, STAGE_SECURITY, STAGE_SECURITY, LoginStatus::Success);
            LoginResult::Continue(pdu)
        }
    }

    fn handle_operational(
        &mut self,
        params: &[(String, String)],
        req: &IscsiPdu,
        transit: bool,
        _nsg: u8,
    ) -> LoginResult {
        // Initiators may start login directly at the operational stage
        // (CSG=1) when no security negotiation is needed — RouterOS does
        // this. Refuse the shortcut when CHAP is required.
        if self.chap_config.is_some() && !self.auth_complete {
            tracing::warn!("Login: operational stage without authentication, but CHAP is required");
            return self.make_error_response(req, LoginStatus::AuthFailure);
        }

        let mut response_params: Vec<(&str, &str)> = Vec::new();

        for (key, val) in params {
            match key.as_str() {
                "InitiatorName" => {
                    self.params.initiator_name = val.clone();
                }
                "TargetName" => {
                    if val != &self.target_name {
                        tracing::warn!("Login: unknown target '{val}'");
                        return self.make_error_response(req, LoginStatus::TargetNotFound);
                    }
                    self.params.target_name = val.clone();
                }
                "SessionType" => {
                    // Declarative (RFC 7143 §12.21): initiator-to-target only.
                    // Echoing it back makes open-iscsi abort the login with
                    // "couldn't recognize text SessionType=Discovery", which
                    // broke SendTargets discovery entirely.
                    self.params.discovery_session = val == "Discovery";
                }
                "HeaderDigest" => {
                    if val.contains("CRC32C") {
                        self.params.header_digest = true;
                        response_params.push(("HeaderDigest", "CRC32C"));
                    } else {
                        self.params.header_digest = false;
                        response_params.push(("HeaderDigest", "None"));
                    }
                }
                "DataDigest" => {
                    if val.contains("CRC32C") {
                        self.params.data_digest = true;
                        response_params.push(("DataDigest", "CRC32C"));
                    } else {
                        self.params.data_digest = false;
                        response_params.push(("DataDigest", "None"));
                    }
                }
                "MaxRecvDataSegmentLength" => {
                    if let Ok(v) = val.parse::<u32>() {
                        self.params.max_recv_data_segment_length = v.min(262144);
                        response_params.push(("MaxRecvDataSegmentLength", val));
                    }
                }
                "MaxBurstLength" => {
                    if let Ok(v) = val.parse::<u32>() {
                        self.params.max_burst_length = v.min(16777215);
                    }
                }
                "FirstBurstLength" => {
                    if let Ok(v) = val.parse::<u32>() {
                        self.params.first_burst_length = v.min(16777215);
                    }
                }
                "InitialR2T" => {
                    self.params.initial_r2t = val == "Yes";
                    response_params.push(("InitialR2T", if self.params.initial_r2t { "Yes" } else { "No" }));
                }
                "ImmediateData" => {
                    self.params.immediate_data = val == "Yes";
                    response_params.push(("ImmediateData", if self.params.immediate_data { "Yes" } else { "No" }));
                }
                "MaxConnections" => {
                    if let Ok(v) = val.parse::<u32>() {
                        self.params.max_connections = v.min(1); // single-conn for now
                        response_params.push(("MaxConnections", "1"));
                    }
                }
                "MaxOutstandingR2T" => {
                    response_params.push(("MaxOutstandingR2T", "1"));
                }
                "DefaultTime2Wait" => {
                    response_params.push(("DefaultTime2Wait", "2"));
                }
                "DefaultTime2Retain" => {
                    response_params.push(("DefaultTime2Retain", "0"));
                }
                "ErrorRecoveryLevel" => {
                    response_params.push(("ErrorRecoveryLevel", "0"));
                }
                "TargetPortalGroupTag" => {
                    response_params.push(("TargetPortalGroupTag", "1"));
                }
                _ => {}
            }
        }

        // Always provide MaxRecvDataSegmentLength from target side
        let max_recv = self.params.max_recv_data_segment_length.to_string();
        let has_max_recv = response_params.iter().any(|(k, _)| *k == "MaxRecvDataSegmentLength");
        if !has_max_recv {
            response_params.push(("MaxRecvDataSegmentLength", &max_recv));
        }

        let resp_data = encode_text_params(&response_params);

        if transit {
            self.stage = STAGE_FULL_FEATURE;
            let pdu = self.make_login_response(req, &resp_data, true, STAGE_OPERATIONAL, STAGE_FULL_FEATURE, LoginStatus::Success);
            LoginResult::Complete(pdu, self.params.clone())
        } else {
            let pdu = self.make_login_response(req, &resp_data, false, STAGE_OPERATIONAL, STAGE_OPERATIONAL, LoginStatus::Success);
            LoginResult::Continue(pdu)
        }
    }

    fn make_login_response(
        &mut self,
        req: &IscsiPdu,
        data: &[u8],
        transit: bool,
        csg: u8,
        nsg: u8,
        status: LoginStatus,
    ) -> IscsiPdu {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::LoginResponse);
        bhs.set_transit(transit);
        bhs.set_csg(csg);
        bhs.set_nsg(nsg);

        // Copy ISID and TSIH from request
        let mut isid = [0u8; 6];
        isid.copy_from_slice(&req.bhs.raw[8..14]);
        bhs.set_isid(&isid);

        bhs.set_initiator_task_tag(req.bhs.initiator_task_tag());

        // Sequence numbers — without these the initiator sees MaxCmdSN=0
        // (a closed command window) and can never issue a SCSI command.
        bhs.set_stat_sn(self.stat_sn);
        bhs.set_exp_cmd_sn(self.exp_cmd_sn);
        bhs.set_max_cmd_sn(self.exp_cmd_sn.wrapping_add(31));
        self.stat_sn = self.stat_sn.wrapping_add(1);

        // Status class/detail in bytes 36-37
        let (class, detail) = status.class_detail();
        bhs.raw[36] = class;
        bhs.raw[37] = detail;

        IscsiPdu::with_data(bhs, data.to_vec())
    }

    fn make_error_response(&mut self, req: &IscsiPdu, status: LoginStatus) -> LoginResult {
        let pdu = self.make_login_response(req, &[], false, self.stage, self.stage, status);
        LoginResult::Failed(pdu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_login_request(csg: u8, nsg: u8, transit: bool, data: &[u8]) -> IscsiPdu {
        let mut bhs = Bhs::new();
        bhs.set_opcode(Opcode::LoginRequest);
        bhs.set_csg(csg);
        bhs.set_nsg(nsg);
        bhs.set_transit(transit);
        bhs.set_initiator_task_tag(1);
        IscsiPdu::with_data(bhs, data.to_vec())
    }

    #[test]
    fn login_no_auth() {
        let mut sm = LoginStateMachine::new("iqn.2024.com.stormblock:disk1".into(), None);

        let params = encode_text_params(&[
            ("InitiatorName", "iqn.2024.com.test:init"),
            ("TargetName", "iqn.2024.com.stormblock:disk1"),
            ("AuthMethod", "None"),
            ("SessionType", "Normal"),
        ]);
        let req = make_login_request(STAGE_SECURITY, STAGE_OPERATIONAL, true, &params);
        let result = sm.process(&req);

        match result {
            LoginResult::Continue(_pdu) => {
                // Now send operational params
                let op_params = encode_text_params(&[
                    ("HeaderDigest", "None"),
                    ("DataDigest", "None"),
                    ("MaxRecvDataSegmentLength", "65536"),
                ]);
                let req2 = make_login_request(STAGE_OPERATIONAL, STAGE_FULL_FEATURE, true, &op_params);
                match sm.process(&req2) {
                    LoginResult::Complete(_, params) => {
                        assert_eq!(params.initiator_name, "iqn.2024.com.test:init");
                        assert!(!params.header_digest);
                    }
                    _ => panic!("expected Complete"),
                }
            }
            LoginResult::Complete(_, params) => {
                // Some implementations go direct to full feature
                assert_eq!(params.initiator_name, "iqn.2024.com.test:init");
            }
            LoginResult::Failed(_) => panic!("login should not fail"),
        }
    }

    /// `SessionType` is declarative — echoing it back makes open-iscsi abort
    /// the login with "couldn't recognize text SessionType=Discovery", which
    /// broke SendTargets discovery outright.
    #[test]
    fn declarative_keys_are_not_echoed() {
        for session_type in ["Discovery", "Normal"] {
            let mut sm = LoginStateMachine::new("iqn.2024.com.stormblock:disk1".into(), None);
            let params = encode_text_params(&[
                ("InitiatorName", "iqn.1994-05.com.redhat:test"),
                ("SessionType", session_type),
                ("AuthMethod", "None"),
            ]);
            let req = make_login_request(STAGE_SECURITY, STAGE_OPERATIONAL, true, &params);
            let pdu = match sm.process(&req) {
                LoginResult::Continue(p) | LoginResult::Complete(p, _) => p,
                LoginResult::Failed(_) => panic!("login should not fail"),
            };
            let echoed = parse_text_params(&pdu.data);
            assert!(
                !echoed.iter().any(|(k, _)| k == "SessionType"),
                "SessionType={session_type} must not be echoed, got {echoed:?}"
            );
            assert!(
                !echoed.iter().any(|(k, _)| k == "InitiatorName"),
                "InitiatorName must not be echoed either"
            );
        }
    }

    /// A discovery session declares no TargetName; it must still log in, and
    /// the session must be marked so it is not treated as a normal one.
    #[test]
    fn discovery_session_is_recognised() {
        let mut sm = LoginStateMachine::new("iqn.2024.com.stormblock:disk1".into(), None);
        let params = encode_text_params(&[
            ("InitiatorName", "iqn.1994-05.com.redhat:test"),
            ("SessionType", "Discovery"),
            ("AuthMethod", "None"),
        ]);
        let req = make_login_request(STAGE_SECURITY, STAGE_FULL_FEATURE, true, &params);
        match sm.process(&req) {
            LoginResult::Complete(_, p) => {
                assert!(p.discovery_session, "session should be flagged as discovery");
                assert_eq!(p.initiator_name, "iqn.1994-05.com.redhat:test");
            }
            LoginResult::Continue(_) => {
                assert!(sm.params.discovery_session, "discovery flag must be recorded");
            }
            LoginResult::Failed(_) => panic!("discovery login must not fail"),
        }
    }

    #[test]
    fn login_wrong_target() {
        let mut sm = LoginStateMachine::new("iqn.2024.com.stormblock:disk1".into(), None);
        let params = encode_text_params(&[
            ("InitiatorName", "iqn.test"),
            ("TargetName", "iqn.wrong"),
        ]);
        let req = make_login_request(STAGE_SECURITY, STAGE_OPERATIONAL, true, &params);
        match sm.process(&req) {
            LoginResult::Failed(_) => {} // expected
            _ => panic!("expected Failed for wrong target"),
        }
    }
}
