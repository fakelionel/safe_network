// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::{
    core::Core,
    error::Result,
    event::Connected,
    id::P2pNode,
    messages::{
        self, BootstrapResponse, JoinRequest, Message, MessageAction, Variant, VerifyStatus,
    },
    relocation::RelocatePayload,
    section::EldersInfo,
    xor_space::Prefix,
};
use bytes::Bytes;
use std::time::Duration;

/// Time after which an attempt to joining a section is cancelled (and possibly retried).
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(60);

// The joining stage - node is waiting to be approved by the section.
pub struct Joining {
    // EldersInfo of the section we are joining.
    elders_info: EldersInfo,
    // Whether we are joining as infant or relocating.
    join_type: JoinType,
    // Token for the join request timeout.
    timer_token: u64,
}

impl Joining {
    pub fn new(
        core: &mut Core,
        elders_info: EldersInfo,
        relocate_payload: Option<RelocatePayload>,
    ) -> Self {
        let join_type = match relocate_payload {
            Some(payload) => JoinType::Relocate(payload),
            None => JoinType::First,
        };
        let timer_token = core.timer.schedule(JOIN_TIMEOUT);

        let stage = Self {
            elders_info,
            join_type,
            timer_token,
        };
        stage.send_join_requests(core);
        stage
    }

    pub fn handle_timeout(&mut self, core: &mut Core, token: u64) {
        if token == self.timer_token {
            debug!("Timeout when trying to join a section");
            // Try again
            self.send_join_requests(core);
            self.timer_token = core.timer.schedule(JOIN_TIMEOUT);
        }
    }

    pub fn decide_message_action(&self, msg: &Message) -> Result<MessageAction> {
        match msg.variant {
            Variant::NodeApproval(_) => {
                match &self.join_type {
                    JoinType::Relocate(payload) => {
                        let details = payload.relocate_details();
                        verify_message(msg, Some(&details.destination_key))?;
                    }
                    JoinType::First { .. } => {
                        // We don't have any trusted keys to verify this message, but we still need to
                        // handle it.
                    }
                }
                Ok(MessageAction::Handle)
            }

            Variant::BootstrapResponse(BootstrapResponse::Join(_)) | Variant::Bounce { .. } => {
                verify_message(msg, None)?;
                Ok(MessageAction::Handle)
            }

            Variant::NeighbourInfo { .. }
            | Variant::UserMessage(_)
            | Variant::GenesisUpdate(_)
            | Variant::Relocate(_)
            | Variant::MessageSignature(_)
            | Variant::BootstrapRequest(_)
            | Variant::BootstrapResponse(_)
            | Variant::JoinRequest(_) => Ok(MessageAction::Bounce),

            Variant::MemberKnowledge { .. }
            | Variant::ParsecRequest(..)
            | Variant::ParsecResponse(..)
            | Variant::Ping => Ok(MessageAction::Discard),
        }
    }

    pub fn create_bounce(&self, msg_bytes: Bytes) -> Variant {
        Variant::Bounce {
            elders_version: None,
            message: msg_bytes,
        }
    }

    pub fn handle_bootstrap_response(
        &mut self,
        core: &mut Core,
        sender: P2pNode,
        new_elders_info: EldersInfo,
    ) -> Result<()> {
        if new_elders_info.version <= self.elders_info.version {
            return Ok(());
        }

        if new_elders_info.prefix.matches(core.name()) {
            info!(
                "Newer Join response for our prefix {:?} from {:?}",
                new_elders_info, sender
            );
            self.elders_info = new_elders_info;
            self.send_join_requests(core);
        } else {
            log_or_panic!(
                log::Level::Error,
                "Newer Join response not for our prefix {:?} from {:?}",
                new_elders_info,
                sender,
            );
        }

        Ok(())
    }

    // The EldersInfo of the section we are joining.
    pub fn target_section_elders_info(&self) -> &EldersInfo {
        &self.elders_info
    }

    // Are we relocating or joining for the first time?
    pub fn connect_type(&self) -> Connected {
        match self.join_type {
            JoinType::First { .. } => Connected::First,
            JoinType::Relocate(_) => Connected::Relocate,
        }
    }

    fn send_join_requests(&self, core: &mut Core) {
        let relocate_payload = match &self.join_type {
            JoinType::First { .. } => None,
            JoinType::Relocate(payload) => Some(payload),
        };

        for dst in self.elders_info.elders.values() {
            let join_request = JoinRequest {
                elders_version: self.elders_info.version,
                relocate_payload: relocate_payload.cloned(),
            };

            let variant = Variant::JoinRequest(Box::new(join_request));

            info!("Sending JoinRequest to {}", dst);
            core.send_direct_message(dst.peer_addr(), variant);
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum JoinType {
    // Node joining the network for the first time.
    First,
    // Node being relocated.
    Relocate(RelocatePayload),
}

fn verify_message(msg: &Message, trusted_key: Option<&bls::PublicKey>) -> Result<()> {
    // The message verification will use only those trusted keys whose prefix is compatible with
    // the message source. By using empty prefix, we make sure `trusted_key` is always used.
    let prefix = Prefix::default();

    msg.verify(trusted_key.map(|key| (&prefix, key)))
        .and_then(VerifyStatus::require_full)
        .map_err(|error| {
            messages::log_verify_failure(msg, &error, trusted_key.map(|key| (&prefix, key)));
            error
        })
}
