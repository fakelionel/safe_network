// Copyright 2021 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

pub(super) mod node_state;
pub(crate) mod section_authority_provider;
pub(super) mod section_keys;
mod section_peers;

#[cfg(test)]
pub(crate) use self::section_authority_provider::test_utils;

pub(super) use self::section_keys::{SectionKeyShare, SectionKeysProvider};

use crate::messaging::{
    system::{ElderCandidates, KeyedSig, NodeState, Peer, Section, SectionAuth, SectionPeers},
    SectionAuthorityProvider,
};
use crate::routing::{
    dkg::SectionAuthUtils,
    error::{Error, Result},
    peer::PeerUtils,
    ELDER_SIZE, RECOMMENDED_SECTION_SIZE,
};
pub(crate) use node_state::NodeStateUtils;
pub(crate) use section_authority_provider::ElderCandidatesUtils;
use section_authority_provider::SectionAuthorityProviderUtils;
pub(super) use section_peers::SectionPeersUtils;
use secured_linked_list::SecuredLinkedList;
use serde::Serialize;
use std::{collections::BTreeSet, convert::TryInto, iter, net::SocketAddr};
use xor_name::{Prefix, XorName};

impl Section {
    /// Creates a minimal `Section` initially containing only info about our elders
    /// (`section_auth`).
    ///
    /// Returns error if `section_auth` is not verifiable with the `chain`.
    pub(super) fn new(
        genesis_key: bls::PublicKey,
        chain: SecuredLinkedList,
        section_auth: SectionAuth<SectionAuthorityProvider>,
    ) -> Result<Self, Error> {
        if section_auth.sig.public_key != *chain.last_key() {
            error!("can't create section: section_auth signed with incorrect key");
            return Err(Error::UntrustedSectionAuthProvider(format!(
                "section key doesn't match last key in proof chain: {:?}",
                section_auth.value
            )));
        }

        if genesis_key != *chain.root_key() {
            return Err(Error::UntrustedProofChain(format!(
                "genesis key doesn't match first key in proof chain: {:?}",
                chain.root_key()
            )));
        }

        // Check if SAP signature is valid
        if !section_auth.self_verify() {
            return Err(Error::UntrustedSectionAuthProvider(format!(
                "invalid signature: {:?}",
                section_auth.value
            )));
        }

        // Check if SAP's section key matches SAP signature's key
        if section_auth.sig.public_key != section_auth.value.public_key_set.public_key() {
            return Err(Error::UntrustedSectionAuthProvider(format!(
                "section key doesn't match signature's key: {:?}",
                section_auth.value
            )));
        }

        // Make sure the proof chain can be trusted,
        // i.e. check each key is signed by its parent/predecesor key.
        if !chain.self_verify() {
            return Err(Error::UntrustedProofChain(format!(
                "invalid chain: {:?}",
                chain
            )));
        }

        Ok(Self {
            genesis_key,
            chain,
            section_auth,
            members: SectionPeers::default(),
        })
    }

    /// Creates `Section` for the first node in the network
    pub(super) fn first_node(peer: Peer) -> Result<(Section, SectionKeyShare)> {
        let secret_key_set = bls::SecretKeySet::random(0, &mut rand::thread_rng());
        let public_key_set = secret_key_set.public_keys();
        let secret_key_share = secret_key_set.secret_key_share(0);

        let section_auth =
            create_first_section_authority_provider(&public_key_set, &secret_key_share, peer)?;

        let mut section = Section::new(
            section_auth.sig.public_key,
            SecuredLinkedList::new(section_auth.sig.public_key),
            section_auth,
        )?;

        for peer in section.section_auth.value.peers() {
            let node_state = NodeState::joined(peer, None);
            let sig = create_first_sig(&public_key_set, &secret_key_share, &node_state)?;
            let _ = section.members.update(SectionAuth {
                value: node_state,
                sig,
            });
        }

        let section_key_share = SectionKeyShare {
            public_key_set,
            index: 0,
            secret_key_share,
        };

        Ok((section, section_key_share))
    }

    pub(super) fn genesis_key(&self) -> &bls::PublicKey {
        &self.genesis_key
    }

    /// Try to merge this `Section` members with `other`. .
    pub(super) fn merge_members(&mut self, members: Option<SectionPeers>) -> Result<()> {
        if let Some(members) = members {
            for info in members {
                let _ = self.update_member(info);
            }
        }

        self.members
            .prune_not_matching(&self.section_auth.value.prefix());

        Ok(())
    }
    /// Try to merge this `Section` with `other`. Returns `InvalidMessage` if `other` is invalid or
    /// its chain is not compatible with the chain of `self`.
    pub(super) fn merge_chain(
        &mut self,
        other: &SectionAuth<SectionAuthorityProvider>,
        proof_chain: SecuredLinkedList,
    ) -> Result<()> {
        // We've been AE validated here.
        self.chain.merge(proof_chain)?;

        if &other.sig.public_key == self.chain.last_key() {
            self.section_auth = other.clone();
        }
        Ok(())
    }

    /// Update the `SectionAuthorityProvider` of our section.
    pub(super) fn update_elders(
        &mut self,
        new_section_auth: SectionAuth<SectionAuthorityProvider>,
        new_key_sig: KeyedSig,
    ) -> bool {
        if new_section_auth.value.prefix() != *self.prefix()
            && !new_section_auth
                .value
                .prefix()
                .is_extension_of(self.prefix())
        {
            return false;
        }

        if !new_section_auth.self_verify() {
            return false;
        }

        // TODO: dont chain insert here
        if let Err(error) = self.chain.insert(
            &new_key_sig.public_key,
            new_section_auth.sig.public_key,
            new_key_sig.signature,
        ) {
            error!(
                "failed to insert key {:?} (signed with {:?}) into the section chain: {:?}",
                new_section_auth.sig.public_key, new_key_sig.public_key, error,
            );
            return false;
        }

        if &new_section_auth.sig.public_key == self.chain.last_key() {
            self.section_auth = new_section_auth;
        }

        self.members
            .prune_not_matching(&self.section_auth.value.prefix());

        true
    }

    /// Update the member. Returns whether it actually changed anything.
    pub(super) fn update_member(&mut self, node_state: SectionAuth<NodeState>) -> bool {
        if !node_state.verify(&self.chain) {
            error!("can't merge member {:?}", node_state.value);
            return false;
        }

        self.members.update(node_state)
    }

    pub(super) fn chain(&self) -> &SecuredLinkedList {
        &self.chain
    }

    pub(super) fn authority_provider(&self) -> &SectionAuthorityProvider {
        &self.section_auth.value
    }

    pub(super) fn section_signed_authority_provider(
        &self,
    ) -> &SectionAuth<SectionAuthorityProvider> {
        &self.section_auth
    }

    pub(super) fn is_elder(&self, name: &XorName) -> bool {
        self.authority_provider().contains_elder(name)
    }

    /// Generate a new section info(s) based on the current set of members,
    /// excluding any member matching a name in the provided `excluded_names` set.
    /// Returns a set of candidate SectionAuthorityProviders.
    pub(super) fn promote_and_demote_elders(
        &self,
        our_name: &XorName,
        excluded_names: &BTreeSet<XorName>,
    ) -> Vec<ElderCandidates> {
        if let Some((our_elder_candidates, other_elder_candidates)) =
            self.try_split(our_name, excluded_names)
        {
            return vec![our_elder_candidates, other_elder_candidates];
        }

        // Candidates for elders out of all the nodes in the section, even out of the
        // relocating nodes if there would not be enough instead.
        let expected_peers =
            self.members
                .elder_candidates(ELDER_SIZE, self.authority_provider(), excluded_names);

        let expected_names: BTreeSet<_> = expected_peers.iter().map(Peer::name).cloned().collect();
        let current_names: BTreeSet<_> = self.authority_provider().names();

        if expected_names == current_names {
            vec![]
        } else if expected_names.len() < crate::routing::supermajority(current_names.len()) {
            warn!("ignore attempt to reduce the number of elders too much");
            vec![]
        } else {
            let elder_candidates =
                ElderCandidates::new(expected_peers, self.authority_provider().prefix());
            vec![elder_candidates]
        }
    }

    // Prefix of our section.
    pub(super) fn prefix(&self) -> &Prefix {
        &self.authority_provider().prefix
    }

    pub(super) fn members(&self) -> &SectionPeers {
        &self.members
    }

    /// Returns members that are either joined or are left but still elders.
    pub(super) fn active_members(&self) -> Box<dyn Iterator<Item = &Peer> + '_> {
        Box::new(
            self.members
                .all()
                .filter(move |info| {
                    self.members.is_joined(info.peer.name()) || self.is_elder(info.peer.name())
                })
                .map(|info| &info.peer),
        )
    }

    /// Returns adults from our section.
    pub(super) fn adults(&self) -> Box<dyn Iterator<Item = &Peer> + '_> {
        Box::new(
            self.members
                .mature()
                .filter(move |peer| !self.is_elder(peer.name())),
        )
    }

    /// Returns live adults from our section.
    pub(super) fn live_adults(&self) -> Box<dyn Iterator<Item = &Peer> + '_> {
        Box::new(self.members.joined().filter_map(move |info| {
            if !self.is_elder(info.peer.name()) {
                Some(&info.peer)
            } else {
                None
            }
        }))
    }

    pub(super) fn find_joined_member_by_addr(&self, addr: &SocketAddr) -> Option<&Peer> {
        self.members
            .joined()
            .find(|info| info.peer.addr() == addr)
            .map(|info| &info.peer)
    }

    // Tries to split our section.
    // If we have enough mature nodes for both subsections, returns the SectionAuthorityProviders
    // of the two subsections. Otherwise returns `None`.
    pub(super) fn try_split(
        &self,
        our_name: &XorName,
        excluded_names: &BTreeSet<XorName>,
    ) -> Option<(ElderCandidates, ElderCandidates)> {
        let next_bit_index = if let Ok(index) = self.prefix().bit_count().try_into() {
            index
        } else {
            // Already at the longest prefix, can't split further.
            return None;
        };

        let next_bit = our_name.bit(next_bit_index);

        let (our_new_size, sibling_new_size) = self
            .members
            .mature()
            .filter(|peer| !excluded_names.contains(peer.name()))
            .map(|peer| peer.name().bit(next_bit_index) == next_bit)
            .fold((0, 0), |(ours, siblings), is_our_prefix| {
                if is_our_prefix {
                    (ours + 1, siblings)
                } else {
                    (ours, siblings + 1)
                }
            });

        // If none of the two new sections would contain enough entries, return `None`.
        if our_new_size < RECOMMENDED_SECTION_SIZE || sibling_new_size < RECOMMENDED_SECTION_SIZE {
            return None;
        }

        let our_prefix = self.prefix().pushed(next_bit);
        let other_prefix = self.prefix().pushed(!next_bit);

        let our_elders = self.members.elder_candidates_matching_prefix(
            &our_prefix,
            ELDER_SIZE,
            self.authority_provider(),
            excluded_names,
        );
        let other_elders = self.members.elder_candidates_matching_prefix(
            &other_prefix,
            ELDER_SIZE,
            self.authority_provider(),
            excluded_names,
        );

        let our_elder_candidates = ElderCandidates::new(our_elders, our_prefix);
        let other_elder_candidates = ElderCandidates::new(other_elders, other_prefix);

        Some((our_elder_candidates, other_elder_candidates))
    }
}

// Create `SectionAuthorityProvider` for the first node.
fn create_first_section_authority_provider(
    pk_set: &bls::PublicKeySet,
    sk_share: &bls::SecretKeyShare,
    mut peer: Peer,
) -> Result<SectionAuth<SectionAuthorityProvider>> {
    peer.set_reachable(true);
    let section_auth =
        SectionAuthorityProvider::new(iter::once(peer), Prefix::default(), pk_set.clone());
    let sig = create_first_sig(pk_set, sk_share, &section_auth)?;
    Ok(SectionAuth::new(section_auth, sig))
}

fn create_first_sig<T: Serialize>(
    pk_set: &bls::PublicKeySet,
    sk_share: &bls::SecretKeyShare,
    payload: &T,
) -> Result<KeyedSig> {
    let bytes = bincode::serialize(payload).map_err(|_| Error::InvalidPayload)?;
    let signature_share = sk_share.sign(&bytes);
    let signature = pk_set
        .combine_signatures(iter::once((0, &signature_share)))
        .map_err(|_| Error::InvalidSignatureShare)?;

    Ok(KeyedSig {
        public_key: pk_set.public_key(),
        signature,
    })
}
