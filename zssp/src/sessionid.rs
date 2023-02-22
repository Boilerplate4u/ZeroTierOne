/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * (c) ZeroTier, Inc.
 * https://www.zerotier.com/
 */

use std::fmt::Display;
use std::num::NonZeroU64;

use zerotier_crypto::random;
use zerotier_utils::memory::{array_range, as_byte_array};

/// 48-bit session ID (most significant 16 bits of u64 are unused)
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SessionId(NonZeroU64); // stored little endian internally

const SESSION_ID_SIZE_BYTES: usize = 6;

impl SessionId {
    pub const SIZE: usize = SESSION_ID_SIZE_BYTES;
    pub const NONE: u64 = 0;
    pub const MAX: u64 = 0xffffffffffff;

    /// Create a new session ID, panicing if 'i' is zero or exceeds MAX.
    pub fn new(i: u64) -> SessionId {
        assert!(i <= Self::MAX);
        Self(NonZeroU64::new(i.to_le()).unwrap())
    }

    /// Create a new random (non-zero) session ID (non-cryptographic PRNG)
    pub fn random() -> Self {
        Self(NonZeroU64::new(((random::xorshift64_random() % (Self::MAX - 1)) + 1).to_le()).unwrap())
    }

    pub(crate) fn new_from_bytes(b: &[u8; Self::SIZE]) -> Option<SessionId> {
        let mut tmp = [0u8; 8];
        tmp[..SESSION_ID_SIZE_BYTES].copy_from_slice(b);
        Self::new_from_u64_le(u64::from_ne_bytes(tmp))
    }

    /// Create from a u64 that is already in little-endian byte order.
    #[inline(always)]
    pub(crate) fn new_from_u64_le(i: u64) -> Option<SessionId> {
        NonZeroU64::new(i & Self::MAX.to_le()).map(|i| Self(i))
    }

    /// Get this session ID as a little-endian byte array.
    #[inline(always)]
    pub(crate) fn as_bytes(&self) -> &[u8; Self::SIZE] {
        array_range::<u8, 8, 0, SESSION_ID_SIZE_BYTES>(as_byte_array(&self.0))
    }
}

impl From<SessionId> for u64 {
    #[inline(always)]
    fn from(sid: SessionId) -> Self {
        u64::from_le(sid.0.get())
    }
}

impl Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:06x}", u64::from_le(self.0.get())))
    }
}
