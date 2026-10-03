//! Subscription traffic / expiry reported by a subscription server's
//! `subscription-userinfo` response header. Type only for now; parsing and
//! presentation come with WP-D.

use serde::{Deserialize, Serialize};

/// One `subscription-userinfo` reading, stored on the profile it came from.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionUsage {
    /// Bytes uploaded in the current billing period.
    pub upload: u64,
    /// Bytes downloaded in the current billing period.
    pub download: u64,
    /// Traffic allowance in bytes; 0 = unlimited / not reported.
    pub total: u64,
    /// Expiry as Unix-epoch seconds; `None` = no expiry reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire: Option<u64>,
    /// When this reading was fetched, Unix-epoch seconds.
    pub fetched_at: u64,
}
