// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

use std::convert::TryFrom;
use std::fmt;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Port on which the daemon will open an API server
pub const DEFAULT_API_PORT: u16 = 12219;

/// Represents a switch ID.  This is a simple wrapper around usize, with checks
/// added to the construction functions to verify that the inner ID is valid.
#[derive(
    Copy, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, JsonSchema,
)]
#[repr(u8)]
pub enum SwitchId {
    Switch0 = 0,
    Switch1,
}

impl fmt::Display for SwitchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Switch{}", *self as u8)
    }
}

impl fmt::Debug for SwitchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", *self)
    }
}

impl From<SwitchId> for u8 {
    fn from(s: SwitchId) -> u8 {
        s as u8
    }
}

impl From<SwitchId> for usize {
    fn from(s: SwitchId) -> usize {
        s as usize
    }
}

impl TryFrom<usize> for SwitchId {
    type Error = &'static str;

    fn try_from(s: usize) -> Result<Self, Self::Error> {
        match s {
            0 => Ok(SwitchId::Switch0),
            1 => Ok(SwitchId::Switch1),
            _ => Err("invalid switch id"),
        }
    }
}

impl TryFrom<u32> for SwitchId {
    type Error = &'static str;

    fn try_from(s: u32) -> Result<Self, Self::Error> {
        SwitchId::try_from(s as usize)
    }
}

impl TryFrom<u8> for SwitchId {
    type Error = &'static str;

    fn try_from(s: u8) -> Result<Self, Self::Error> {
        SwitchId::try_from(s as usize)
    }
}
