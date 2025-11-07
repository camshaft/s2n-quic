// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::credentials::Credentials;

impl_tag!(FLOW_RESET);
impl_packet!(FlowReset, {
    #[inline]
    pub const fn queue_id(&self) -> VarInt {
        self.value.queue_id
    }

    #[inline]
    pub const fn tag(&self) -> Tag {
        Tag(Tag::VALUE)
    }

    #[inline]
    pub const fn credentials(&self) -> &Credentials {
        &self.value.credentials
    }

    #[inline]
    pub fn credential_id(&self) -> &crate::credentials::Id {
        &self.value.credentials.id
    }
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(bolero_generator::TypeGenerator))]
pub struct FlowReset {
    pub credentials: Credentials,
    pub wire_version: WireVersion,
    pub queue_id: VarInt,
    pub code: VarInt,
}

impl FlowReset {
    #[inline]
    pub fn encode<C>(&self, mut encoder: EncoderBuffer, crypto: &C) -> usize
    where
        C: seal::control::Secret,
    {
        encoder.encode(&Tag::default());
        encoder.encode(&self.credentials);
        encoder.encode(&self.wire_version);
        encoder.encode(&self.queue_id);
        encoder.encode(&self.code);

        encoder::finish(encoder, crypto)
    }

    #[cfg(test)]
    fn validate(&self) -> Option<()> {
        Some(())
    }
}

impl<'a> DecoderValue<'a> for FlowReset {
    #[inline]
    fn decode(buffer: DecoderBuffer<'a>) -> R<'a, Self> {
        let (_tag, buffer) = buffer.decode::<Tag>()?;
        let (credentials, buffer) = buffer.decode()?;
        let (wire_version, buffer) = buffer.decode()?;
        let (queue_id, buffer) = buffer.decode()?;
        let (code, buffer) = buffer.decode()?;
        let value = Self {
            wire_version,
            credentials,
            queue_id,
            code,
        };
        Ok((value, buffer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
}
