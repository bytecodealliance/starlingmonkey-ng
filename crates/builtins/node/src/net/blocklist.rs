// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `net.BlockList` and `net.SocketAddress`.
//!
//! A `SocketAddress` keeps its address and a `BlockList` its rules as their private data.
//! Addresses parse and format as libuv's `uv_ip4_addr`, `uv_ip6_addr` and `uv_inet_ntop` do, and
//! the rules match as Node.js's `SocketAddressBlockList` matches them. An IPv6 zone ID is accepted
//! and ignored.

use std::cmp::Ordering;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{validate_integer, validate_plain_object, validate_string, ErrorKind};
use crate::errors::{NodeError, Thrown};
use crate::support::array_elements;

/// An address as Node.js's `SocketAddress` holds it. Two addresses are equal if their IPs, ports
/// and flow labels are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SockAddr {
    ip: IpAddr,
    port: u16,
    /// The flow label, which is 0 for IPv4.
    flowlabel: u32,
}

impl Default for SockAddr {
    fn default() -> Self {
        Self {
            ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
            flowlabel: 0,
        }
    }
}

/// The first 12 bytes of an IPv4-mapped IPv6 address.
const MAPPED_PREFIX: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff];

/// The bytes of `ip`, with an IPv4 address as its IPv4-mapped IPv6 address.
fn mapped(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(ip) => ip.to_ipv6_mapped().octets(),
        IpAddr::V6(ip) => ip.octets(),
    }
}

/// `v4` compared with `v6`, or `None` if `v6` isn't IPv4-mapped.
fn compare_v4_v6(v4: Ipv4Addr, v6: Ipv6Addr) -> Option<Ordering> {
    let bytes = v6.octets();
    (bytes[..12] == MAPPED_PREFIX).then(|| v4.octets()[..].cmp(&bytes[12..]))
}

/// `a` compared with `b`. An IPv4 address and an IPv6 address are ordered only if the IPv6 address
/// is IPv4-mapped.
fn compare(a: IpAddr, b: IpAddr) -> Option<Ordering> {
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => Some(a.cmp(&b)),
        (IpAddr::V4(a), IpAddr::V6(b)) => compare_v4_v6(a, b),
        (IpAddr::V6(a), IpAddr::V4(b)) => compare_v4_v6(b, a).map(Ordering::reverse),
        (IpAddr::V6(a), IpAddr::V6(b)) => Some(a.octets().cmp(&b.octets())),
    }
}

/// Whether the first `prefix` bits of `a` and `b` are equal.
fn prefix_matches(a: &[u8; 16], b: &[u8; 16], prefix: u32) -> bool {
    let whole = (prefix / 8) as usize;
    let rest = prefix % 8;
    if a[..whole] != b[..whole] {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    a[whole] & mask == b[whole] & mask
}

/// `text` as libuv's `uv_ip4_addr` parses an IPv4 address: text after a NUL is ignored.
fn parse_ipv4(text: &str) -> Option<Ipv4Addr> {
    text.split('\0').next()?.parse().ok()
}

/// `text` as libuv's `uv_ip6_addr` parses an IPv6 address: text after a NUL is ignored, and so is
/// a zone ID after `%`, which leaves at most 39 bytes of address.
fn parse_ipv6(text: &str) -> Option<Ipv6Addr> {
    let text = text.split('\0').next()?;
    let address = match text.split_once('%') {
        Some((address, _)) => {
            std::str::from_utf8(&address.as_bytes()[..address.len().min(39)]).ok()?
        }
        None => text,
    };
    address.parse().ok()
}

/// `ip` as libuv's `uv_inet_ntop` formats an IPv6 address: the first longest run of at least two
/// zero groups is `::`, and the last four bytes of an IPv4-mapped address, or of an address whose
/// first six groups are zero and seventh isn't, are in dotted decimal.
pub(crate) fn format_ipv6(ip: Ipv6Addr) -> String {
    let words = ip.segments();
    let bytes = ip.octets();
    let mut best: Option<(usize, usize)> = None;
    let mut current: Option<(usize, usize)> = None;
    for (index, word) in words.iter().enumerate() {
        if *word == 0 {
            current = Some(match current {
                Some((base, len)) => (base, len + 1),
                None => (index, 1),
            });
        } else if let Some(run) = current.take() {
            if best.is_none_or(|best| run.1 > best.1) {
                best = Some(run);
            }
        }
    }
    if let Some(run) = current {
        if best.is_none_or(|best| run.1 > best.1) {
            best = Some(run);
        }
    }
    let best = best.filter(|&(_, len)| len >= 2);
    let mut out = String::new();
    for (index, word) in words.iter().enumerate() {
        if let Some((base, len)) = best {
            if (base..base + len).contains(&index) {
                if index == base {
                    out.push(':');
                }
                continue;
            }
        }
        if index != 0 {
            out.push(':');
        }
        let dotted =
            index == 6 && (best == Some((0, 6)) || (best == Some((0, 5)) && words[5] == 0xffff));
        if dotted {
            out.push_str(&Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]).to_string());
            return out;
        }
        out.push_str(&format!("{word:x}"));
    }
    if best.is_some_and(|(base, len)| base + len == 8) {
        out.push(':');
    }
    out
}

impl SockAddr {
    /// The IP as `SocketAddress`'s `address` shows it.
    fn address(&self) -> String {
        match self.ip {
            IpAddr::V4(ip) => ip.to_string(),
            IpAddr::V6(ip) => format_ipv6(ip),
        }
    }

    /// The family as the rules show it, `IPv4` or `IPv6`.
    fn family_label(&self) -> &'static str {
        if self.ip.is_ipv4() {
            "IPv4"
        } else {
            "IPv6"
        }
    }

    /// The family as `SocketAddress`'s `family` shows it, `ipv4` or `ipv6`.
    fn family(&self) -> &'static str {
        if self.ip.is_ipv4() {
            "ipv4"
        } else {
            "ipv6"
        }
    }
}

/// The rules of a `BlockList`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Rules {
    /// The blocked addresses in the order they were first added, each under its bytes as
    /// [`mapped`] gives them. An IPv4 address and its IPv4-mapped IPv6 address are one rule, which
    /// shows the address added last.
    addresses: Vec<([u8; 16], SockAddr)>,
    /// The blocked subnets and their prefix lengths, the latest first.
    subnets: Vec<(SockAddr, u32)>,
    /// The blocked ranges, the latest first.
    ranges: Vec<(SockAddr, SockAddr)>,
}

impl Rules {
    fn add_address(&mut self, address: SockAddr) {
        let key = mapped(address.ip);
        match self.addresses.iter_mut().find(|(other, _)| *other == key) {
            Some(entry) => entry.1 = address,
            None => self.addresses.push((key, address)),
        }
    }

    fn remove_address(&mut self, address: SockAddr) {
        let key = mapped(address.ip);
        self.addresses.retain(|(other, _)| *other != key);
    }

    fn remove_range(&mut self, start: SockAddr, end: SockAddr) {
        if let Some(index) = self.ranges.iter().position(|range| *range == (start, end)) {
            self.ranges.remove(index);
        }
    }

    fn remove_subnet(&mut self, network: SockAddr, prefix: u32) {
        if let Some(index) = self
            .subnets
            .iter()
            .position(|subnet| *subnet == (network, prefix))
        {
            self.subnets.remove(index);
        }
    }

    /// Whether a rule blocks `ip`. An IPv4 address and its IPv4-mapped IPv6 address match the same
    /// addresses and subnets. A range matches the addresses [`compare`] orders within it.
    pub(crate) fn blocks(&self, ip: IpAddr) -> bool {
        let bytes = mapped(ip);
        if self.addresses.iter().any(|(key, _)| *key == bytes) {
            return true;
        }
        let in_subnet = self.subnets.iter().any(|(network, prefix)| {
            let prefix = if network.ip.is_ipv4() {
                prefix + 96
            } else {
                *prefix
            };
            prefix_matches(&bytes, &mapped(network.ip), prefix)
        });
        in_subnet
            || self.ranges.iter().any(|(start, end)| {
                matches!(
                    compare(ip, start.ip),
                    Some(Ordering::Greater | Ordering::Equal)
                ) && matches!(compare(ip, end.ip), Some(Ordering::Less | Ordering::Equal))
            })
    }

    /// The rules as `BlockList`'s `rules` shows them: addresses, then subnets, then ranges.
    fn describe(&self) -> Vec<String> {
        let addresses = self.addresses.iter().map(|(_, address)| {
            format!("Address: {} {}", address.family_label(), address.address())
        });
        let subnets = self.subnets.iter().map(|(network, prefix)| {
            format!(
                "Subnet: {} {}/{prefix}",
                network.family_label(),
                network.address()
            )
        });
        let ranges = self.ranges.iter().map(|(start, end)| {
            format!(
                "Range: {} {}-{}",
                start.family_label(),
                start.address(),
                end.address()
            )
        });
        addresses.chain(subnets).chain(ranges).collect()
    }

    fn size(&self) -> usize {
        self.addresses.len() + self.subnets.len() + self.ranges.len()
    }
}

/// `value` as a `SocketAddress`, if it is one.
fn as_socket_address<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Option<SocketAddress<'s>> {
    Object::from_value(scope, *value)
        .ok()
        .and_then(|object| object.cast::<SocketAddress>().ok())
}

/// The address of `value`, if it is a `SocketAddress`.
fn socket_address_of(scope: &Scope<'_>, value: HandleValue<'_>) -> Option<SockAddr> {
    as_socket_address(scope, value).map(|address| address.data().address)
}

/// `value` as a `BlockList`, if it is one.
fn as_block_list<'s>(scope: &'s Scope<'_>, value: HandleValue<'_>) -> Option<BlockList<'s>> {
    Object::from_value(scope, *value)
        .ok()
        .and_then(|object| object.cast::<BlockList>().ok())
}

/// The rules of `value`, if it is a `BlockList`.
pub(crate) fn rules_of(scope: &Scope<'_>, value: HandleValue<'_>) -> Option<Rules> {
    as_block_list(scope, value).map(|list| list.data().rules.clone())
}

/// Whether `value` is a `BlockList` that blocks `ip`.
pub(crate) fn blocks(scope: &Scope<'_>, value: HandleValue<'_>, ip: IpAddr) -> bool {
    as_block_list(scope, value).is_some_and(|list| list.data().rules.blocks(ip))
}

/// `ERR_INVALID_ARG_TYPE` naming `name` unless `value` is a `BlockList`, as `net.Socket` and
/// `net.Server` check their `blockList` option.
pub(crate) fn validate_block_list(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<(), Thrown> {
    if as_block_list(scope, value).is_none() {
        return Err(NodeError::invalid_arg_type(scope, name, "an net.BlockList", value).into());
    }
    Ok(())
}

/// The address of the `SocketAddress` `this`, or a `TypeError` if it isn't a `SocketAddress`.
fn this_address(scope: &Scope<'_>, this: HandleValue<'_>) -> Result<SockAddr, Thrown> {
    socket_address_of(scope, this).ok_or_else(|| {
        js::error::TypeError("The receiver is not a SocketAddress".into()).throw(scope);
        Thrown::Pending
    })
}

/// Whether `family`, the `family` option of a `SocketAddress`, lowercased with its `toLowerCase`
/// method if it has one, is `ipv4` (`Some(false)`) or `ipv6` (`Some(true)`).
fn family_of(scope: &Scope<'_>, family: HandleValue<'_>) -> Result<Option<bool>, ExnThrown> {
    let lowered = if family.is_string() {
        Some(String::from_jsval_throwing(scope, family, ())?)
    } else if let Ok(object) = Object::from_value(scope, *family) {
        let method = object.get_property(scope, c"toLowerCase")?;
        if js::function::is_callable(method) {
            let lowered = Function::call(scope, family, method, js::function::EmptyArgs)?;
            lowered
                .is_string()
                .then(|| String::from_jsval_throwing(scope, lowered, ()))
                .transpose()?
        } else {
            None
        }
    } else {
        None
    };
    // Only ASCII letters lowercase to the ASCII letters of `ipv4` and `ipv6`.
    Ok(match lowered {
        Some(name) if name.eq_ignore_ascii_case("ipv4") => Some(false),
        Some(name) if name.eq_ignore_ascii_case("ipv6") => Some(true),
        _ => None,
    })
}

/// The address with the options `address`, `port` and `flowlabel`, IPv6 if `ipv6` and IPv4
/// otherwise, as the `SocketAddress` constructor validates them.
///
/// A flow label above 0xFFFFF for IPv6, or above 2**31 - 1 for IPv4, throws `ERR_OUT_OF_RANGE`,
/// where Node.js aborts.
fn make_address(
    scope: &Scope<'_>,
    ipv6: bool,
    address: HandleValue<'_>,
    port: HandleValue<'_>,
    flowlabel: HandleValue<'_>,
) -> Result<SockAddr, Thrown> {
    let text = validate_string(scope, address, "options.address")?;
    let port = super::validate_port(scope, port, "options.port")?;
    let label = validate_integer(scope, flowlabel, "options.flowlabel", 0.0, 4294967295.0)?;
    let max = if ipv6 { 1048575.0 } else { 2147483647.0 };
    if label > max {
        return Err(NodeError::out_of_range(
            "options.flowlabel",
            &format!(">= 0 && <= {max}"),
            &crate::errors::out_of_range_received(label),
        )
        .into());
    }
    let ip = if ipv6 {
        parse_ipv6(&text).map(IpAddr::V6)
    } else {
        parse_ipv4(&text).map(IpAddr::V4)
    };
    let ip = ip.ok_or_else(|| {
        NodeError::new(
            ErrorKind::Error,
            "ERR_INVALID_ADDRESS",
            "Invalid socket address",
        )
    })?;
    Ok(SockAddr {
        ip,
        port,
        flowlabel: if ipv6 { label as u32 } else { 0 },
    })
}

/// The address `value` gives a `BlockList` method: a `SocketAddress`, or the string `value`, named
/// `name`, in the family `family` (default `ipv4`).
fn address_arg(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    family: Option<HandleValue<'_>>,
) -> Result<SockAddr, Thrown> {
    if let Some(address) = socket_address_of(scope, value) {
        return Ok(address);
    }
    validate_string(scope, value, name)?;
    let family = family_or_default(scope, family)?;
    validate_string(scope, family, "family")?;
    let Some(ipv6) = family_of(scope, family)? else {
        return Err(
            NodeError::invalid_arg_value(scope, "options.family", family, "is invalid").into(),
        );
    };
    let zero = scope.root_value(js::value::from_f64(0.0));
    make_address(scope, ipv6, value, zero, zero)
}

/// `family`, or `ipv4` if it is missing.
fn family_or_default<'s>(
    scope: &'s Scope<'_>,
    family: Option<HandleValue<'_>>,
) -> Result<HandleValue<'s>, ExnThrown> {
    match family {
        Some(family) => Ok(scope.root_value(family.get())),
        None => js::class::get_or_init_atom(scope, c"ipv4"),
    }
}

/// The prefix length `prefix` for `network`, as `addSubnet` and `removeSubnet` validate it.
fn prefix_arg(
    scope: &Scope<'_>,
    network: SockAddr,
    prefix: HandleValue<'_>,
) -> Result<u32, Thrown> {
    let max = if network.ip.is_ipv4() { 32.0 } else { 128.0 };
    Ok(validate_integer(scope, prefix, "prefix", 0.0, max)? as u32)
}

/// The address and prefix length of the CIDR string `cidr`, as Node.js's `parseCIDR` splits it:
/// the address is IPv6 if it contains a `:`.
fn parse_cidr(scope: &Scope<'_>, cidr: HandleValue<'_>) -> Result<(String, f64, bool), Thrown> {
    let text = validate_string(scope, cidr, "cidr")?;
    let Some((address, prefix)) = text.rsplit_once('/') else {
        return Err(NodeError::invalid_arg_value(
            scope,
            "cidr",
            cidr,
            "must contain a prefix length (e.g. \"10.0.0.0/8\")",
        )
        .into());
    };
    let digits = prefix.strip_prefix('-').unwrap_or(prefix);
    let number = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| prefix.parse::<f64>().ok())
        .flatten()
        .filter(|number| crate::errors::number_to_string(*number) == prefix);
    let Some(number) = number else {
        return Err(NodeError::invalid_arg_value(
            scope,
            "cidr",
            cidr,
            "prefix length must be a valid integer",
        )
        .into());
    };
    Ok((address.to_string(), number, address.contains(':')))
}

/// `addSubnet(network, prefix, family)` on `list`.
fn add_subnet(
    scope: &Scope<'_>,
    list: &BlockList<'_>,
    network: HandleValue<'_>,
    prefix: HandleValue<'_>,
    family: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let network = address_arg(scope, network, "network", family)?;
    let prefix = prefix_arg(scope, network, prefix)?;
    list.data_mut().rules.subnets.insert(0, (network, prefix));
    Ok(())
}

/// `removeSubnet(network, prefix, family)` on `list`.
fn remove_subnet(
    scope: &Scope<'_>,
    list: &BlockList<'_>,
    network: HandleValue<'_>,
    prefix: HandleValue<'_>,
    family: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let network = address_arg(scope, network, "network", family)?;
    let prefix = prefix_arg(scope, network, prefix)?;
    list.data_mut().rules.remove_subnet(network, prefix);
    Ok(())
}

/// `addSubnet` for the address, prefix length and family `parse_cidr` gives, or `removeSubnet`
/// if `remove`.
fn add_cidr(
    scope: &Scope<'_>,
    list: &BlockList<'_>,
    (address, prefix, ipv6): (String, f64, bool),
    remove: bool,
) -> Result<(), Thrown> {
    let network = address.as_str().to_jsval_throwing(scope)?;
    let prefix = scope.root_value(js::value::from_f64(prefix));
    let family = js::class::get_or_init_atom(scope, if ipv6 { c"ipv6" } else { c"ipv4" })?;
    if remove {
        remove_subnet(scope, list, network, prefix, Some(family))
    } else {
        add_subnet(scope, list, network, prefix, Some(family))
    }
}

/// `addRange(start, end, family)` on `list`, with the `SocketAddress`es or strings `start` and
/// `end`.
fn add_range(
    scope: &Scope<'_>,
    list: &BlockList<'_>,
    start: HandleValue<'_>,
    end: HandleValue<'_>,
    family: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let start_address = address_arg(scope, start, "start", family)?;
    let end_address = address_arg(scope, end, "end", family)?;
    if compare(start_address.ip, end_address.ip) == Some(Ordering::Greater) {
        let start = match socket_address_of(scope, start) {
            Some(_) => start,
            None => new_socket_address(scope, start_address)?.to_jsval_throwing(scope)?,
        };
        return Err(
            NodeError::invalid_arg_value(scope, "start", start, "must come before end").into(),
        );
    }
    list.data_mut()
        .rules
        .ranges
        .insert(0, (start_address, end_address));
    Ok(())
}

/// A new `SocketAddress` of `address`.
fn new_socket_address<'s>(
    scope: &'s Scope<'_>,
    address: SockAddr,
) -> Result<SocketAddress<'s>, ExnThrown> {
    js::class::create_instance_with::<SocketAddressImpl>(scope, |_| SocketAddressImpl { address })
}

/// The rules of `list` as an array of strings.
fn rules_array<'s>(scope: &'s Scope<'_>, list: &BlockList<'_>) -> Result<js::Array<'s>, ExnThrown> {
    let rules = list.data().rules.describe();
    let values = rules
        .iter()
        .map(|rule| rule.as_str().to_jsval_throwing(scope))
        .collect::<Result<Vec<_>, _>>()?;
    js::Array::with_contents(scope, &values)
}

/// A rule string `fromJSON` adds: a subnet, an address or a range, each in the family `ipv6`
/// (`true`) or `ipv4`.
enum JsonRule {
    Subnet(String, f64, bool),
    Address(String, bool),
    Range(String, String, bool),
}

/// The length of the run of bytes at the start of `text` that are in `class`.
fn run(text: &[u8], class: impl Fn(u8) -> bool) -> usize {
    text.iter().take_while(|byte| class(**byte)).count()
}

/// The length of the match of `\d{1,3}(?:\.\d{1,3}){3}` at the start of `text`: the longest one,
/// or one that `next` follows.
fn dotted(text: &[u8], next: Option<u8>) -> Option<usize> {
    let mut at = 0;
    for octet in 0..4 {
        let digits = run(&text[at..], |byte| byte.is_ascii_digit());
        let follow = if octet < 3 { Some(b'.') } else { next };
        match follow {
            Some(follow) if digits == 0 || digits > 3 || text.get(at + digits) != Some(&follow) => {
                return None
            }
            Some(_) if octet < 3 => at += digits + 1,
            Some(_) => at += digits,
            None if digits == 0 => return None,
            None => at += digits.min(3),
        }
    }
    Some(at)
}

/// The length of the match of `[0-9a-fA-F:]{1,39}` at the start of `text`: the longest one, or
/// one that `next` follows.
fn hex(text: &[u8], next: Option<u8>) -> Option<usize> {
    let length = run(text, |byte| byte.is_ascii_hexdigit() || byte == b':');
    match next {
        _ if length == 0 => None,
        Some(next) => (length <= 39 && text.get(length) == Some(&next)).then_some(length),
        None => Some(length.min(39)),
    }
}

/// The digits of `\d{1,max}` at the start of `text`, as a number.
fn digits(text: &[u8], max: usize) -> Option<f64> {
    let length = run(text, |byte| byte.is_ascii_digit()).min(max);
    (length > 0).then(|| {
        text[..length]
            .iter()
            .fold(0.0, |number, digit| number * 10.0 + f64::from(digit - b'0'))
    })
}

/// The first match in `text` of `literal`, compared ignoring ASCII case if `fold`, followed by
/// what `rest` matches.
fn search<T>(
    text: &[u8],
    literal: &str,
    fold: bool,
    rest: impl Fn(&[u8]) -> Option<T>,
) -> Option<T> {
    let literal = literal.as_bytes();
    (0..text.len().saturating_sub(literal.len() - 1)).find_map(|start| {
        let candidate = text.get(start..start + literal.len())?;
        let found = if fold {
            candidate.eq_ignore_ascii_case(literal)
        } else {
            candidate == literal
        };
        if found {
            rest(&text[start + literal.len()..])
        } else {
            None
        }
    })
}

/// The text of `text[..length]`.
fn text_of(text: &[u8], length: usize) -> String {
    String::from_utf8_lossy(&text[..length]).into_owned()
}

/// The rule `item` describes, as Node.js's `#parseIPInfo` matches the rule strings `rules`
/// produces, or `None` if it describes none.
fn json_rule(item: &str) -> Option<JsonRule> {
    let text = item.as_bytes();
    if item.contains("IPv4") {
        let subnet = search(text, "Subnet: IPv4 ", false, |rest| {
            let network = dotted(rest, Some(b'/'))?;
            Some(JsonRule::Subnet(
                text_of(rest, network),
                digits(&rest[network + 1..], 2)?,
                false,
            ))
        });
        let address = || {
            search(text, "Address: IPv4 ", false, |rest| {
                Some(JsonRule::Address(text_of(rest, dotted(rest, None)?), false))
            })
        };
        let range = || {
            search(text, "Range: IPv4 ", false, |rest| {
                let start = dotted(rest, Some(b'-'))?;
                let end = &rest[start + 1..];
                Some(JsonRule::Range(
                    text_of(rest, start),
                    text_of(end, dotted(end, None)?),
                    false,
                ))
            })
        };
        if let Some(rule) = subnet.or_else(address).or_else(range) {
            return Some(rule);
        }
    }
    if item.contains("IPv6") {
        let subnet = search(text, "Subnet: IPv6 ", true, |rest| {
            let network = hex(rest, Some(b'/'))?;
            Some(JsonRule::Subnet(
                text_of(rest, network),
                digits(&rest[network + 1..], 3)?,
                true,
            ))
        });
        let address = || {
            search(text, "Address: IPv6 ", true, |rest| {
                Some(JsonRule::Address(text_of(rest, hex(rest, None)?), true))
            })
        };
        let range = || {
            search(text, "Range: IPv6 ", true, |rest| {
                let start = hex(rest, Some(b'-'))?;
                let end = &rest[start + 1..];
                Some(JsonRule::Range(
                    text_of(rest, start),
                    text_of(end, hex(end, None)?),
                    true,
                ))
            })
        };
        return subnet.or_else(address).or_else(range);
    }
    None
}

/// The strings `fromJSON(data)` reads rules from: `data` if it is an array of strings, or the
/// array of strings the JSON string `data` holds.
fn json_items(scope: &Scope<'_>, data: HandleValue<'_>) -> Result<Vec<String>, Thrown> {
    let invalid = |value: HandleValue<'_>| {
        Thrown::from(NodeError::invalid_arg_type(
            scope,
            "data",
            "of type string or string[]",
            value,
        ))
    };
    let array = if data.is_string() {
        let text = js::JSString::from_value(scope, data)?;
        js::json::parse_js_string(scope, text.handle())?
    } else {
        data
    };
    let items = array_elements(scope, array)?.ok_or_else(|| invalid(array))?;
    let mut strings = Vec::with_capacity(items.len());
    for item in items {
        if !item.is_string() {
            return Err(invalid(array));
        }
        strings.push(String::from_jsval_throwing(scope, item, ())?);
    }
    Ok(strings)
}

/// The body of a custom `util.inspect` method: `[name]` at a negative depth, and otherwise
/// `name` followed by `util.inspect` of what `shown` gives, one level deeper.
fn inspect_as<'s>(
    scope: &'s Scope<'_>,
    args: &CallbackArgs<'_>,
    name: &str,
    shown: impl FnOnce(&'s Scope<'_>, HandleValue<'_>) -> Result<Object<'s>, Thrown>,
) -> Result<Value, ExnThrown> {
    let this = scope.root_value(args.this());
    let depth = args.get(0);
    if depth.is_number() && depth.to_number() < 0.0 {
        // Node.js returns the object, which it shows as `[name]` because of its own symbol key.
        return format!("[{name}]").as_str().to_jsval_raw_throwing(scope);
    }
    let mut options = crate::inspect::InspectOptions::current(scope)?;
    let depth = match Object::from_value(scope, args.get(1)) {
        Ok(given) => {
            options = options.read(scope, given)?;
            given.get_property(scope, c"depth")?
        }
        Err(_) => HandleValue::undefined(),
    };
    let depth = if depth.is_null() || depth.is_undefined() {
        HandleValue::null()
    } else {
        let depth = f64::from_jsval_throwing(scope, depth, ())? - 1.0;
        scope.root_value(js::value::from_f64(depth))
    };
    options.set(scope, c"depth", depth)?;
    let value = shown(scope, this).map_err(|error| error.throw(scope))?;
    let text = crate::inspect::inspect(scope, value.to_jsval_throwing(scope)?, options)?;
    format!("{name} {text}")
        .as_str()
        .to_jsval_raw_throwing(scope)
}

/// `BlockList.prototype[util.inspect.custom]`.
fn inspect_block_list(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    inspect_as(scope, &args, "BlockList", |scope, this| {
        let list = as_block_list(scope, this).ok_or_else(|| {
            js::error::TypeError("The receiver is not a BlockList".into()).throw(scope);
            Thrown::Pending
        })?;
        let shown = Object::new_plain(scope)?;
        shown.set_property(scope, c"rules", *rules_array(scope, &list)?)?;
        Ok(shown)
    })
}

/// `SocketAddress.prototype[util.inspect.custom]`.
fn inspect_socket_address(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<Value, ExnThrown> {
    inspect_as(scope, &args, "SocketAddress", |scope, this| {
        let address = this_address(scope, this)?;
        Ok(socket_address_json(scope, address)?)
    })
}

/// `address` as `SocketAddress`'s `toJSON` gives it.
fn socket_address_json<'s>(
    scope: &'s Scope<'_>,
    address: SockAddr,
) -> Result<Object<'s>, ExnThrown> {
    let json = Object::new_plain(scope)?;
    json.set_property(scope, c"address", address.address().as_str())?;
    json.set_property(scope, c"port", u32::from(address.port))?;
    json.set_property(scope, c"family", address.family())?;
    json.set_property(scope, c"flowlabel", address.flowlabel)?;
    Ok(json)
}

/// Define `object[util.inspect.custom]` as `body`.
fn define_inspect(
    scope: &Scope<'_>,
    object: Object<'_>,
    body: js::function::Callback,
) -> Result<(), ExnThrown> {
    let symbol = crate::inspect::INSPECT_CUSTOM.value(scope)?;
    let function = Function::new_callback(scope, c"[nodejs.util.inspect.custom]", 2, body, ())?;
    let key = scope.root_id(js::id::value_to_id(scope, symbol)?);
    object.set_property_by_id(scope, key, (*function).to_jsval_throwing(scope)?)
}

/// The CIDR ranges of the private, loopback, link-local and unique local addresses.
const PRIVATE_RANGES: [&str; 8] = [
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "127.0.0.0/8",
    "::1/128",
    "169.254.0.0/16",
    "fe80::/10",
    "fc00::/7",
];

/// `value`, or `undefined` for a missing argument.
fn arg<'a>(value: Option<HandleValue<'a>>) -> HandleValue<'a> {
    value.unwrap_or(HandleValue::undefined())
}

/// <https://nodejs.org/api/net.html#class-netsocketaddress>
#[core_runtime::jsclass(hidden)]
pub struct SocketAddress {
    #[no_trace]
    address: SockAddr,
}

#[core_runtime::jsmethods]
impl SocketAddress {
    /// <https://nodejs.org/api/net.html#new-netsocketaddressoptions>: the address
    /// `options.address` (default `127.0.0.1`, or `::` if `options.family` isn't `ipv4`) of the
    /// family `options.family` (default `ipv4`), with `options.port` and `options.flowlabel`.
    #[constructor]
    fn new(&self, scope: &Scope<'_>, options: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        let options = match options.filter(|options| !options.is_undefined()) {
            Some(options) => validate_plain_object(scope, options, "options", false)?,
            None => Object::new_plain(scope)?,
        };
        let family = options.get_property(scope, c"family")?;
        let family = if family.is_undefined() {
            js::class::get_or_init_atom(scope, c"ipv4")?
        } else {
            family
        };
        let address = options.get_property(scope, c"address")?;
        let address = if !address.is_undefined() {
            address
        } else if family.is_string() && String::from_jsval_throwing(scope, family, ())? == "ipv4" {
            js::class::get_or_init_atom(scope, c"127.0.0.1")?
        } else {
            js::class::get_or_init_atom(scope, c"::")?
        };
        let zero = scope.root_value(js::value::from_f64(0.0));
        let port = options.get_property(scope, c"port")?;
        let port = if port.is_undefined() { zero } else { port };
        let flowlabel = options.get_property(scope, c"flowlabel")?;
        let flowlabel = if flowlabel.is_undefined() {
            zero
        } else {
            flowlabel
        };
        let Some(ipv6) = family_of(scope, family)? else {
            let family = options.get_property(scope, c"family")?;
            return Err(NodeError::invalid_arg_value(
                scope,
                "options.family",
                family,
                "is invalid",
            )
            .into());
        };
        self.data_mut().address = make_address(scope, ipv6, address, port, flowlabel)?;
        Ok(())
    }

    #[getter]
    fn address(&self) -> String {
        self.data().address.address()
    }

    #[getter]
    fn port(&self) -> f64 {
        f64::from(self.data().address.port)
    }

    #[getter]
    fn family(&self) -> &'static str {
        self.data().address.family()
    }

    #[getter]
    fn flowlabel(&self) -> f64 {
        f64::from(self.data().address.flowlabel)
    }

    /// <https://nodejs.org/api/net.html#socketaddresstojson>
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    fn to_json<'r>(&self, scope: &'r Scope<'_>) -> Result<Object<'r>, ExnThrown> {
        socket_address_json(scope, self.data().address)
    }

    /// Whether `value` is a `SocketAddress`.
    #[static_method(length = 1)]
    fn is_socket_address(scope: &Scope<'_>, value: Option<HandleValue<'_>>) -> bool {
        socket_address_of(scope, arg(value)).is_some()
    }

    /// <https://nodejs.org/api/net.html#socketaddressparseinput>: the address and port of the
    /// host of the URL `http://<input>`, or `undefined` if that isn't a valid URL with an IP
    /// address. The port of a URL that names port 80 is 0.
    #[static_method(length = 1)]
    fn parse<'s>(
        scope: &'s Scope<'_>,
        input: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let input = validate_string(scope, arg(input), "input")?;
        let Ok(url) = url::Url::parse(&format!("http://{input}")) else {
            return Ok(HandleValue::undefined());
        };
        let host = url::quirks::hostname(&url);
        let port = url::quirks::port(&url).parse::<u16>().unwrap_or(0);
        let (host, ipv6) = match host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
        {
            Some(host) => (host, true),
            None => (host, false),
        };
        let ip = if ipv6 {
            parse_ipv6(host).map(IpAddr::V6)
        } else {
            parse_ipv4(host).map(IpAddr::V4)
        };
        match ip {
            Some(ip) => {
                let address = SockAddr {
                    ip,
                    port,
                    flowlabel: 0,
                };
                Ok(new_socket_address(scope, address)?.to_jsval_throwing(scope)?)
            }
            None => Ok(HandleValue::undefined()),
        }
    }
}

/// <https://nodejs.org/api/net.html#class-netblocklist>
#[core_runtime::jsclass(hidden)]
pub struct BlockList {
    #[no_trace]
    rules: Rules,
}

#[core_runtime::jsmethods]
impl BlockList {
    /// <https://nodejs.org/api/net.html#class-netblocklist>: a list without rules.
    #[constructor]
    fn construct() -> Self {
        Self {
            rules: Rules::default(),
        }
    }

    /// Whether `value` is a `BlockList`.
    #[static_method(length = 1)]
    fn is_block_list(scope: &Scope<'_>, value: Option<HandleValue<'_>>) -> bool {
        as_block_list(scope, arg(value)).is_some()
    }

    /// <https://nodejs.org/api/net.html#blocklistrules>
    #[getter]
    fn rules<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Array<'r>, ExnThrown> {
        rules_array(scope, self)
    }

    /// The number of rules.
    #[getter]
    fn size(&self) -> f64 {
        self.data().rules.size() as f64
    }

    /// <https://nodejs.org/api/net.html#blocklistaddaddressaddress-family>
    #[method(length = 1)]
    fn add_address(
        &self,
        scope: &Scope<'_>,
        address: Option<HandleValue<'_>>,
        family: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let address = address_arg(scope, arg(address), "address", family)?;
        self.data_mut().rules.add_address(address);
        Ok(())
    }

    /// Block each of `addresses`, `SocketAddress`es or strings in the family `family`, once all
    /// of them are valid.
    #[method(length = 1)]
    fn add_addresses(
        &self,
        scope: &Scope<'_>,
        addresses: Option<HandleValue<'_>>,
        family: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let addresses = arg(addresses);
        let Some(items) = array_elements(scope, addresses)? else {
            return Err(NodeError::invalid_arg_type(
                scope,
                "addresses",
                "an instance of Array",
                addresses,
            )
            .into());
        };
        let family = family_or_default(scope, family)?;
        validate_string(scope, family, "family")?;
        let mut parsed = Vec::new();
        for (index, address) in items.into_iter().enumerate() {
            let name = format!("addresses[{index}]");
            parsed.push(address_arg(scope, address, &name, Some(family))?);
        }
        let mut data = self.data_mut();
        for address in parsed {
            data.rules.add_address(address);
        }
        Ok(())
    }

    /// <https://nodejs.org/api/net.html#blocklistaddrangestart-end-family>
    #[method(length = 2)]
    fn add_range(
        &self,
        scope: &Scope<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
        family: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        add_range(scope, self, arg(start), arg(end), family)
    }

    /// <https://nodejs.org/api/net.html#blocklistaddsubnetnet-prefix-family>
    #[method(length = 2)]
    fn add_subnet(
        &self,
        scope: &Scope<'_>,
        network: Option<HandleValue<'_>>,
        prefix: Option<HandleValue<'_>>,
        family: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        add_subnet(scope, self, arg(network), arg(prefix), family)
    }

    /// Block the subnet `cidr`, such as `10.0.0.0/8`, which is IPv6 if it contains a `:`.
    #[method(name = "addCIDR", length = 1)]
    fn add_cidr(&self, scope: &Scope<'_>, cidr: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        let cidr = parse_cidr(scope, arg(cidr))?;
        add_cidr(scope, self, cidr, false)
    }

    /// `addCIDR` for each of `cidrs`, once all of them are CIDR strings.
    #[method(name = "addCIDRs", length = 1)]
    fn add_cidrs(&self, scope: &Scope<'_>, cidrs: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        let cidrs = arg(cidrs);
        let Some(items) = array_elements(scope, cidrs)? else {
            return Err(
                NodeError::invalid_arg_type(scope, "cidrs", "an instance of Array", cidrs).into(),
            );
        };
        let mut parsed = Vec::new();
        for (index, cidr) in items.into_iter().enumerate() {
            validate_string(scope, cidr, &format!("cidrs[{index}]"))?;
            parsed.push(parse_cidr(scope, cidr)?);
        }
        for cidr in parsed {
            add_cidr(scope, self, cidr, false)?;
        }
        Ok(())
    }

    /// Remove the rule blocking `address`, and the one blocking its IPv4 or IPv4-mapped IPv6
    /// counterpart.
    #[method(length = 1)]
    fn remove_address(
        &self,
        scope: &Scope<'_>,
        address: Option<HandleValue<'_>>,
        family: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let address = address_arg(scope, arg(address), "address", family)?;
        self.data_mut().rules.remove_address(address);
        Ok(())
    }

    /// Remove the latest range rule from `start` to `end`, whose addresses, ports and flow labels
    /// are equal to these.
    #[method(length = 2)]
    fn remove_range(
        &self,
        scope: &Scope<'_>,
        start: Option<HandleValue<'_>>,
        end: Option<HandleValue<'_>>,
        family: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        let start = address_arg(scope, arg(start), "start", family)?;
        let end = address_arg(scope, arg(end), "end", family)?;
        self.data_mut().rules.remove_range(start, end);
        Ok(())
    }

    /// Remove the latest subnet rule of `network` and `prefix`.
    #[method(length = 2)]
    fn remove_subnet(
        &self,
        scope: &Scope<'_>,
        network: Option<HandleValue<'_>>,
        prefix: Option<HandleValue<'_>>,
        family: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        remove_subnet(scope, self, arg(network), arg(prefix), family)
    }

    /// `removeSubnet` for the subnet `cidr`.
    #[method(name = "removeCIDR", length = 1)]
    fn remove_cidr(&self, scope: &Scope<'_>, cidr: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        let cidr = parse_cidr(scope, arg(cidr))?;
        add_cidr(scope, self, cidr, true)
    }

    /// <https://nodejs.org/api/net.html#blocklistcheckaddress-family>: a string address is IPv4
    /// if `family` is `ipv4` in any case, and IPv6 otherwise. An invalid one isn't blocked.
    #[method(length = 1)]
    fn check(
        &self,
        scope: &Scope<'_>,
        address: Option<HandleValue<'_>>,
        family: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        let address = arg(address);
        let ip = match socket_address_of(scope, address) {
            Some(address) => Some(address.ip),
            None => {
                let text = validate_string(scope, address, "address")?;
                let family = validate_string(scope, family_or_default(scope, family)?, "family")?;
                if family.eq_ignore_ascii_case("ipv4") {
                    parse_ipv4(&text).map(IpAddr::V4)
                } else {
                    parse_ipv6(&text).map(IpAddr::V6)
                }
            }
        };
        Ok(ip.is_some_and(|ip| self.data().rules.blocks(ip)))
    }

    /// Remove every rule.
    #[method]
    fn clear(&self) {
        self.data_mut().rules = Rules::default();
    }

    /// The rules, as `rules` shows them.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "toJSON")]
    fn to_json<'r>(&self, scope: &'r Scope<'_>) -> Result<js::Array<'r>, ExnThrown> {
        rules_array(scope, self)
    }

    /// Add the rules described by `data`, an array of the strings `rules` produces or that array
    /// as JSON. Strings that describe no rule are skipped.
    #[allow(clippy::wrong_self_convention)]
    #[method(name = "fromJSON", length = 1)]
    fn from_json(&self, scope: &Scope<'_>, data: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        for item in json_items(scope, arg(data))? {
            let ipv6 = |ipv6: bool| {
                ipv6.then(|| js::class::get_or_init_atom(scope, c"ipv6"))
                    .transpose()
            };
            match json_rule(&item) {
                Some(JsonRule::Subnet(network, prefix, is_ipv6)) => {
                    let network = network.as_str().to_jsval_throwing(scope)?;
                    let prefix = scope.root_value(js::value::from_f64(prefix));
                    add_subnet(scope, self, network, prefix, ipv6(is_ipv6)?)?;
                }
                Some(JsonRule::Address(address, is_ipv6)) => {
                    let address = address.as_str().to_jsval_throwing(scope)?;
                    let address = address_arg(scope, address, "address", ipv6(is_ipv6)?)?;
                    self.data_mut().rules.add_address(address);
                }
                Some(JsonRule::Range(start, end, is_ipv6)) => {
                    let start = start.as_str().to_jsval_throwing(scope)?;
                    let end = end.as_str().to_jsval_throwing(scope)?;
                    add_range(scope, self, start, end, ipv6(is_ipv6)?)?;
                }
                None => {}
            }
        }
        Ok(())
    }
}

/// Register `BlockList` and `SocketAddress` on `global`, with their inspection methods and
/// `BlockList.PRIVATE_RANGES`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    BlockList::add_to_global(scope, global);
    SocketAddress::add_to_global(scope, global);
    let proto = js::class::get_prototype_object_for::<BlockListImpl>(scope)
        .expect("registering the class creates its prototype");
    define_inspect(scope, proto, inspect_block_list)?;
    let ranges = js::Array::with_contents(scope, &PRIVATE_RANGES)?;
    ranges.freeze(scope)?;
    BlockList::class_constructor(scope)?.define_property(
        scope,
        c"PRIVATE_RANGES",
        *ranges,
        js::class_spec::JSPROP_ENUMERATE as u32,
    )?;
    let proto = js::class::get_prototype_object_for::<SocketAddressImpl>(scope)
        .expect("registering the class creates its prototype");
    define_inspect(scope, proto, inspect_socket_address)
}
