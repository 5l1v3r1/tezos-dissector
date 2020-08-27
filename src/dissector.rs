// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

use wireshark_epan_adapter::{
    Dissector,
    dissector::{DissectorHelper, Tree, PacketInfo},
};
use std::collections::BTreeMap;
use super::{conversation::Context, identity::Identity};

pub struct TezosDissector {
    identity: Option<Identity>,
    // Each pair of endpoints has its own context.
    // The pair is unordered,
    // so A talk to B is the same conversation as B talks to A.
    // The key is just pointer in memory, so it is invalid when capturing session is closed.
    contexts: BTreeMap<usize, ContextExt>,
}

pub struct ContextExt {
    inner: Context,
    frame_result: Result<(), u64>,
}

impl ContextExt {
    pub fn new(inner: Context) -> Self {
        ContextExt {
            inner,
            frame_result: Ok(()),
        }
    }

    /// The context becomes invalid if the inner is invalid or
    /// if the decryption error occurs in some previous frame.
    /// If the frame number is equal to the frame where error occurs,
    /// the context still valid, but after that it is invalid.
    /// Let's show the error message once.
    fn invalid(&self, frame_number: u64) -> bool {
        let error = match &self.frame_result {
            &Ok(()) => false,
            &Err(f) => frame_number > f,
        };
        error || self.inner.invalid()
    }

    pub fn visualize(
        &mut self,
        packet_length: usize,
        packet_info: &PacketInfo,
        root: &mut Tree,
    ) -> usize {
        // the context might become invalid if the conversation is not tezos,
        // or if decryption error occurs
        if !self.invalid(packet_info.frame_number()) {
            let r = self.inner.visualize(packet_length, packet_info, root);
            self.frame_result = r;
            packet_length
        } else {
            0
        }
    }
}

impl TezosDissector {
    pub fn new() -> Self {
        TezosDissector {
            identity: None,
            contexts: BTreeMap::new(),
        }
    }
}

impl Dissector for TezosDissector {
    // This method called by the wireshark when the user choose the identity file.
    fn prefs_update(&mut self, filenames: Vec<&str>) {
        if let Some(identity_path) = filenames.first().cloned() {
            if !identity_path.is_empty() {
                // read the identity from the file
                self.identity = Identity::from_path(identity_path)
                    .map_err(|e| {
                        log::error!("Identity: {}", e);
                        e
                    })
                    .ok();
            }
        }
    }

    // This method called by the wireshark when a new packet just arrive,
    // or when the user click on the packet.
    fn consume(
        &mut self,
        helper: &mut DissectorHelper,
        root: &mut Tree,
        packet_info: &PacketInfo,
    ) -> usize {
        // get the data
        let payload = helper.payload();
        // retrieve or create a new context for the conversation
        let context_key = helper.context_key(packet_info);
        let context = self
            .contexts
            .entry(context_key)
            .or_insert_with(|| ContextExt::new(Context::new(packet_info)));
        if !packet_info.visited() {
            // consume each packet only once
            context
                .inner
                .consume(payload.as_ref(), packet_info, self.identity.as_ref());
        }
        context.visualize(payload.len(), packet_info, root)
    }

    // This method called by the wireshark when the user
    // closing current capturing session
    fn cleanup(&mut self) {
        self.contexts.clear();
    }
}
