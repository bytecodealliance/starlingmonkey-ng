// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! DNS messages for `dns.Resolver`, encoded and decoded with `hickory-proto`: query names as
//! c-ares reads them, queries, and the records of responses as c-ares and Node.js's `cares_wrap`
//! parse them.

use std::fmt::Write as _;
use std::net::IpAddr;

use hickory_proto::op::{Message, MessageType, OpCode, Query};
use hickory_proto::rr::{DNSClass, Name, RData, Record, RecordType};

/// The c-ares status of a failed query, with the error code Node.js gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    NoData,
    FormErr,
    ServFail,
    NotFound,
    NotImp,
    Refused,
    BadName,
    BadResp,
    ConnRefused,
    Timeout,
    NoMem,
    Cancelled,
    /// The channel has no servers. Node.js has no code for this status.
    NoServer,
}

impl Status {
    /// The error code, as Node.js's `ToErrorCodeString` names the status.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Status::NoData => "ENODATA",
            Status::FormErr => "EFORMERR",
            Status::ServFail => "ESERVFAIL",
            Status::NotFound => "ENOTFOUND",
            Status::NotImp => "ENOTIMP",
            Status::Refused => "EREFUSED",
            Status::BadName => "EBADNAME",
            Status::BadResp => "EBADRESP",
            Status::ConnRefused => "ECONNREFUSED",
            Status::Timeout => "ETIMEOUT",
            Status::NoMem => "ENOMEM",
            Status::Cancelled => "ECANCELLED",
            Status::NoServer => "UNKNOWN_ARES_ERROR",
        }
    }
}

/// What a resolver method queries, in the order of `Resolver.prototype`'s methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueryKind {
    Any,
    A,
    Aaaa,
    Caa,
    Cname,
    Mx,
    Ns,
    Tlsa,
    Txt,
    Srv,
    Ptr,
    Naptr,
    Soa,
    Reverse,
}

impl QueryKind {
    pub(crate) const ALL: [QueryKind; 14] = [
        QueryKind::Any,
        QueryKind::A,
        QueryKind::Aaaa,
        QueryKind::Caa,
        QueryKind::Cname,
        QueryKind::Mx,
        QueryKind::Ns,
        QueryKind::Tlsa,
        QueryKind::Txt,
        QueryKind::Srv,
        QueryKind::Ptr,
        QueryKind::Naptr,
        QueryKind::Soa,
        QueryKind::Reverse,
    ];

    /// The name of the `cares_wrap` binding, which is the `syscall` of its errors and the name of
    /// the resolver method.
    pub(crate) fn binding_name(self) -> &'static str {
        match self {
            QueryKind::Any => "queryAny",
            QueryKind::A => "queryA",
            QueryKind::Aaaa => "queryAaaa",
            QueryKind::Caa => "queryCaa",
            QueryKind::Cname => "queryCname",
            QueryKind::Mx => "queryMx",
            QueryKind::Ns => "queryNs",
            QueryKind::Tlsa => "queryTlsa",
            QueryKind::Txt => "queryTxt",
            QueryKind::Srv => "querySrv",
            QueryKind::Ptr => "queryPtr",
            QueryKind::Naptr => "queryNaptr",
            QueryKind::Soa => "querySoa",
            QueryKind::Reverse => "getHostByAddr",
        }
    }

    /// The name of the resolver method.
    pub(crate) fn method_name(self) -> &'static str {
        match self {
            QueryKind::Any => "resolveAny",
            QueryKind::A => "resolve4",
            QueryKind::Aaaa => "resolve6",
            QueryKind::Caa => "resolveCaa",
            QueryKind::Cname => "resolveCname",
            QueryKind::Mx => "resolveMx",
            QueryKind::Ns => "resolveNs",
            QueryKind::Tlsa => "resolveTlsa",
            QueryKind::Txt => "resolveTxt",
            QueryKind::Srv => "resolveSrv",
            QueryKind::Ptr => "resolvePtr",
            QueryKind::Naptr => "resolveNaptr",
            QueryKind::Soa => "resolveSoa",
            QueryKind::Reverse => "reverse",
        }
    }

    /// The kind that `resolve(hostname, rrtype)` queries for `rrtype`.
    pub(crate) fn from_rrtype(rrtype: &str) -> Option<QueryKind> {
        Some(match rrtype {
            "ANY" => QueryKind::Any,
            "A" => QueryKind::A,
            "AAAA" => QueryKind::Aaaa,
            "CAA" => QueryKind::Caa,
            "CNAME" => QueryKind::Cname,
            "MX" => QueryKind::Mx,
            "NS" => QueryKind::Ns,
            "TLSA" => QueryKind::Tlsa,
            "TXT" => QueryKind::Txt,
            "SRV" => QueryKind::Srv,
            "PTR" => QueryKind::Ptr,
            "NAPTR" => QueryKind::Naptr,
            "SOA" => QueryKind::Soa,
            _ => return None,
        })
    }

    /// The record type queried.
    pub(crate) fn record_type(self) -> RecordType {
        match self {
            QueryKind::Any => RecordType::ANY,
            QueryKind::A => RecordType::A,
            QueryKind::Aaaa => RecordType::AAAA,
            QueryKind::Caa => RecordType::CAA,
            QueryKind::Cname => RecordType::CNAME,
            QueryKind::Mx => RecordType::MX,
            QueryKind::Ns => RecordType::NS,
            QueryKind::Tlsa => RecordType::TLSA,
            QueryKind::Txt => RecordType::TXT,
            QueryKind::Srv => RecordType::SRV,
            QueryKind::Ptr | QueryKind::Reverse => RecordType::PTR,
            QueryKind::Naptr => RecordType::NAPTR,
            QueryKind::Soa => RecordType::SOA,
        }
    }
}

/// Whether c-ares accepts `byte` in a query name: `[A-Za-z0-9-*._/]`.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'*' | b'.' | b'_' | b'/')
}

/// The query name for `name`, as Node.js and c-ares read it. The text before the first NUL is
/// converted to ASCII with UTS #46, which yields the root name if the conversion fails. A name
/// ending in `.onion` or `.onion.` is `ENOTFOUND`. The name is then split into labels at the dots,
/// with `\` escaping the next character, or the byte of a three-digit decimal number. A label must
/// be 1 to 63 bytes from `[A-Za-z0-9-*._/]`, the name at most 255 bytes encoded, and a trailing dot
/// is ignored. `""` and `"."` are the root name. Other names are `EBADNAME`.
pub(crate) fn query_name(name: &str) -> Result<Name, Status> {
    let name = name.split('\0').next().unwrap_or_default();
    let ascii = idna::domain_to_ascii(name).unwrap_or_default();
    let lower = ascii.to_ascii_lowercase();
    if lower.ends_with(".onion") || lower.ends_with(".onion.") {
        return Err(Status::NotFound);
    }
    if ascii.is_empty() || ascii == "." {
        return Ok(Name::root());
    }
    let mut labels: Vec<Vec<u8>> = Vec::new();
    let mut label = Vec::new();
    let mut bytes = ascii.bytes().peekable();
    while let Some(byte) = bytes.next() {
        let byte = match byte {
            b'.' => {
                if label.is_empty() {
                    return Err(Status::BadName);
                }
                labels.push(std::mem::take(&mut label));
                continue;
            }
            b'\\' => match bytes.next() {
                Some(digit) if digit.is_ascii_digit() => {
                    let mut value = u32::from(digit - b'0');
                    for _ in 0..2 {
                        match bytes.next() {
                            Some(digit) if digit.is_ascii_digit() => {
                                value = value * 10 + u32::from(digit - b'0');
                            }
                            _ => return Err(Status::BadName),
                        }
                    }
                    u8::try_from(value).map_err(|_| Status::BadName)?
                }
                Some(escaped) => escaped,
                None => return Err(Status::BadName),
            },
            byte => byte,
        };
        if !is_name_byte(byte) || label.len() == 63 {
            return Err(Status::BadName);
        }
        label.push(byte);
    }
    if !label.is_empty() {
        labels.push(label);
    }
    Name::from_labels(labels).map_err(|_| Status::BadName)
}

/// The name that `reverse` queries for `address`, under `in-addr.arpa` or `ip6.arpa`.
pub(crate) fn reverse_name(address: IpAddr) -> Name {
    Name::from(address)
}

/// A query with the ID `id` for the records of type `record_type` of `name`, with recursion
/// desired, as c-ares sends it.
pub(crate) fn encode_query(
    id: u16,
    name: &Name,
    record_type: RecordType,
) -> Result<Vec<u8>, Status> {
    let mut message = Message::new(id, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = true;
    message.add_query(Query::query(name.clone(), record_type));
    message.to_vec().map_err(|_| Status::BadName)
}

/// What a datagram or TCP message received for a query is.
pub(crate) enum Reply {
    /// It is the response to another query, by its ID or its question.
    Other,
    /// It doesn't parse, or has no single question.
    Malformed,
    Answer(Message),
}

/// What `bytes` is for the query with the ID `id` for `name` and `record_type`.
pub(crate) fn check_reply(bytes: &[u8], id: u16, name: &Name, record_type: RecordType) -> Reply {
    if bytes.len() < 2 || u16::from_be_bytes([bytes[0], bytes[1]]) != id {
        return Reply::Other;
    }
    let Ok(message) = Message::from_vec(bytes) else {
        return Reply::Malformed;
    };
    if message.metadata.message_type != MessageType::Response || message.queries.len() != 1 {
        return Reply::Malformed;
    }
    let query = &message.queries[0];
    if query.query_type() != record_type
        || query.query_class() != DNSClass::IN
        || query.name() != name
    {
        return Reply::Other;
    }
    Reply::Answer(message)
}

/// The status of the response `message`, from its response code, as c-ares sets it. A response
/// without error and with an empty answer section is `ENODATA`. Unknown response codes are no error.
pub(crate) fn response_status(message: &Message) -> Result<(), Status> {
    match u16::from(message.metadata.response_code) & 0xf {
        0 if message.answers.is_empty() => Err(Status::NoData),
        1 => Err(Status::FormErr),
        2 => Err(Status::ServFail),
        3 => Err(Status::NotFound),
        4 => Err(Status::NotImp),
        5 => Err(Status::Refused),
        _ => Ok(()),
    }
}

/// `name` as c-ares writes it: its labels joined by dots, without a trailing dot, so the root
/// name is empty. `"`, `.`, `;`, `\`, `(`, `)`, `@` and `$` in a label are escaped with `\`, and
/// bytes outside the printable ASCII range are written as `\` and three decimal digits.
pub(crate) fn name_text(name: &Name) -> String {
    let mut text = String::new();
    for (index, label) in name.iter().enumerate() {
        if index > 0 {
            text.push('.');
        }
        for &byte in label {
            match byte {
                b'"' | b'.' | b';' | b'\\' | b'(' | b')' | b'@' | b'$' => {
                    text.push('\\');
                    text.push(char::from(byte));
                }
                0x20..=0x7e => text.push(char::from(byte)),
                _ => {
                    let _ = write!(text, "\\{byte:03}");
                }
            }
        }
    }
    text
}

/// The bytes of `text` before its first NUL, as c-ares's NUL-terminated strings hold them.
fn until_nul(text: &[u8]) -> Vec<u8> {
    text.split(|&byte| byte == 0)
        .next()
        .unwrap_or_default()
        .to_vec()
}

/// An address and its TTL.
pub(crate) struct Address {
    pub(crate) address: IpAddr,
    pub(crate) ttl: u32,
}

pub(crate) struct Mx {
    pub(crate) exchange: String,
    pub(crate) priority: u16,
}

pub(crate) struct Srv {
    pub(crate) name: String,
    pub(crate) port: u16,
    pub(crate) priority: u16,
    pub(crate) weight: u16,
}

/// A NAPTR record, whose strings are bytes, each a character of the JS string.
pub(crate) struct Naptr {
    pub(crate) flags: Vec<u8>,
    pub(crate) service: Vec<u8>,
    pub(crate) regexp: Vec<u8>,
    pub(crate) replacement: String,
    pub(crate) order: u16,
    pub(crate) preference: u16,
}

pub(crate) struct Soa {
    pub(crate) nsname: String,
    pub(crate) hostmaster: String,
    pub(crate) serial: u32,
    pub(crate) refresh: i32,
    pub(crate) retry: i32,
    pub(crate) expire: i32,
    pub(crate) minttl: u32,
}

/// A CAA record: its flags, and its tag and value, each up to its first NUL.
pub(crate) struct Caa {
    pub(crate) critical: u8,
    pub(crate) tag: Vec<u8>,
    pub(crate) value: Vec<u8>,
}

pub(crate) struct Tlsa {
    pub(crate) cert_usage: u8,
    pub(crate) selector: u8,
    pub(crate) matching: u8,
    pub(crate) data: Vec<u8>,
}

/// A record of a `resolveAny` result.
pub(crate) enum AnyRecord {
    A(Address),
    Aaaa(Address),
    Cname(String),
    Mx(Mx),
    Ns(String),
    Txt(Vec<Vec<u8>>),
    Srv(Srv),
    Ptr(String),
    Naptr(Naptr),
    Soa(Soa),
    Tlsa(Tlsa),
    Caa(Caa),
}

/// The result of a query, as the resolver method gives it. `None` entries are holes in the
/// array, which Node.js leaves for answer records that are not TLSA records in TLSA results.
pub(crate) enum Answer {
    Addresses(Vec<Address>),
    Names(Vec<String>),
    Mx(Vec<Mx>),
    Txt(Vec<Vec<Vec<u8>>>),
    Srv(Vec<Srv>),
    Naptr(Vec<Naptr>),
    Soa(Soa),
    Caa(Vec<Caa>),
    Tlsa(Vec<Option<Tlsa>>),
    Any(Vec<Option<AnyRecord>>),
}

/// The addresses of the A records, or the AAAA records if `v6`, of the answer section, as
/// `ares_parse_a_reply` and `ares_parse_aaaa_reply` give them: every address record of the
/// family, each with the lower of its TTL and the lowest TTL of the CNAME records. `ENODATA`
/// unless there is an address or a CNAME record.
fn addresses(answers: &[Record], v6: bool) -> Result<Vec<Address>, Status> {
    let cname_ttl = answers
        .iter()
        .filter(|record| matches!(record.data, RData::CNAME(_)))
        .map(|record| record.ttl)
        .min();
    let mut found = Vec::new();
    for record in answers {
        let address = match (&record.data, v6) {
            (RData::A(a), false) => IpAddr::V4(a.0),
            (RData::AAAA(aaaa), true) => IpAddr::V6(aaaa.0),
            _ => continue,
        };
        let ttl = cname_ttl.map_or(record.ttl, |cname_ttl| record.ttl.min(cname_ttl));
        found.push(Address { address, ttl });
    }
    if found.is_empty() && cname_ttl.is_none() {
        return Err(Status::NoData);
    }
    Ok(found)
}

/// The target of the first CNAME record of the answer section.
fn first_cname(answers: &[Record]) -> Option<String> {
    answers.iter().find_map(|record| match &record.data {
        RData::CNAME(cname) => Some(name_text(&cname.0)),
        _ => None,
    })
}

/// The targets of the NS records, or of the PTR records if `ptr`, of the answer section. `ENODATA` if
/// there are none.
fn names(answers: &[Record], ptr: bool) -> Result<Vec<String>, Status> {
    let found: Vec<String> = answers
        .iter()
        .filter_map(|record| match (&record.data, ptr) {
            (RData::NS(ns), false) => Some(name_text(&ns.0)),
            (RData::PTR(target), true) => Some(name_text(&target.0)),
            _ => None,
        })
        .collect();
    if found.is_empty() {
        return Err(Status::NoData);
    }
    Ok(found)
}

fn mx_records(answers: &[Record]) -> Vec<Mx> {
    answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::MX(mx) => Some(Mx {
                exchange: name_text(&mx.exchange),
                priority: mx.preference,
            }),
            _ => None,
        })
        .collect()
}

/// The strings of each TXT record.
fn txt_records(answers: &[Record]) -> Vec<Vec<Vec<u8>>> {
    answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::TXT(txt) if !txt.txt_data.is_empty() => {
                Some(txt.txt_data.iter().map(|text| text.to_vec()).collect())
            }
            _ => None,
        })
        .collect()
}

fn srv_records(answers: &[Record]) -> Vec<Srv> {
    answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::SRV(srv) => Some(Srv {
                name: name_text(&srv.target),
                port: srv.port,
                priority: srv.priority,
                weight: srv.weight,
            }),
            _ => None,
        })
        .collect()
}

/// The NAPTR records. A NUL in one of their strings is `EBADRESP`, as c-ares rejects it.
fn naptr_records(answers: &[Record]) -> Result<Vec<Naptr>, Status> {
    let mut found = Vec::new();
    for record in answers {
        let RData::NAPTR(naptr) = &record.data else {
            continue;
        };
        let strings = [&naptr.flags, &naptr.services, &naptr.regexp];
        if strings.iter().any(|text| text.contains(&0)) {
            return Err(Status::BadResp);
        }
        found.push(Naptr {
            flags: naptr.flags.to_vec(),
            service: naptr.services.to_vec(),
            regexp: naptr.regexp.to_vec(),
            replacement: name_text(&naptr.replacement),
            order: naptr.order,
            preference: naptr.preference,
        });
    }
    Ok(found)
}

/// The first SOA record of the answer section.
fn soa_record(answers: &[Record]) -> Option<Soa> {
    answers.iter().find_map(|record| match &record.data {
        RData::SOA(soa) => Some(Soa {
            nsname: name_text(&soa.mname),
            hostmaster: name_text(&soa.rname),
            serial: soa.serial,
            refresh: soa.refresh,
            retry: soa.retry,
            expire: soa.expire,
            minttl: soa.minimum,
        }),
        _ => None,
    })
}

fn caa_records(answers: &[Record]) -> Vec<Caa> {
    answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::CAA(caa) => Some(Caa {
                critical: caa.flags(),
                tag: until_nul(caa.tag.as_bytes()),
                value: until_nul(&caa.value),
            }),
            _ => None,
        })
        .collect()
}

/// The TLSA records with data, at the index of their answer.
fn tlsa_records(answers: &[Record]) -> Vec<(usize, Tlsa)> {
    answers
        .iter()
        .enumerate()
        .filter_map(|(index, record)| match &record.data {
            RData::TLSA(tlsa) if !tlsa.cert_data.is_empty() => Some((
                index,
                Tlsa {
                    cert_usage: u8::from(tlsa.cert_usage),
                    selector: u8::from(tlsa.selector),
                    matching: u8::from(tlsa.matching),
                    data: tlsa.cert_data.clone(),
                },
            )),
            _ => None,
        })
        .collect()
}

/// Place `records` at `offset` plus their index in `entries`, leaving holes before them.
fn place<T>(entries: &mut Vec<Option<T>>, offset: usize, records: Vec<(usize, T)>) {
    for (index, record) in records {
        let at = offset + index;
        if entries.len() <= at {
            entries.resize_with(at + 1, || None);
        }
        entries[at] = Some(record);
    }
}

/// `result`, with `ENODATA` as no records.
fn or_none<T>(result: Result<Vec<T>, Status>) -> Result<Vec<T>, Status> {
    match result {
        Err(Status::NoData) => Ok(Vec::new()),
        result => result,
    }
}

/// The records of a `resolveAny` result, in the order Node.js's `AnyTraits::Parse` collects
/// them: the A records, or the first CNAME target if there are CNAME records, then the AAAA, MX,
/// NS, TXT, SRV, PTR and NAPTR records, the first SOA record, and the TLSA and CAA records.
fn any_records(answers: &[Record]) -> Result<Vec<Option<AnyRecord>>, Status> {
    let mut entries: Vec<Option<AnyRecord>> = Vec::new();
    match (addresses(answers, false), first_cname(answers)) {
        (Err(Status::NoData), _) => {}
        (Err(status), _) => return Err(status),
        (Ok(_), Some(target)) => entries.push(Some(AnyRecord::Cname(target))),
        (Ok(a), None) => entries.extend(a.into_iter().map(|a| Some(AnyRecord::A(a)))),
    }
    let aaaa = or_none(addresses(answers, true))?;
    entries.extend(aaaa.into_iter().map(|a| Some(AnyRecord::Aaaa(a))));
    entries.extend(
        mx_records(answers)
            .into_iter()
            .map(|mx| Some(AnyRecord::Mx(mx))),
    );
    let ns = or_none(names(answers, false))?;
    entries.extend(ns.into_iter().map(|ns| Some(AnyRecord::Ns(ns))));
    entries.extend(
        txt_records(answers)
            .into_iter()
            .map(|txt| Some(AnyRecord::Txt(txt))),
    );
    entries.extend(
        srv_records(answers)
            .into_iter()
            .map(|srv| Some(AnyRecord::Srv(srv))),
    );
    let ptr = or_none(names(answers, true))?;
    entries.extend(ptr.into_iter().map(|ptr| Some(AnyRecord::Ptr(ptr))));
    let naptr = naptr_records(answers)?;
    entries.extend(naptr.into_iter().map(|naptr| Some(AnyRecord::Naptr(naptr))));
    if let Some(soa) = soa_record(answers) {
        entries.push(Some(AnyRecord::Soa(soa)));
    }
    let offset = entries.len();
    let tlsa = tlsa_records(answers)
        .into_iter()
        .map(|(index, tlsa)| (index, AnyRecord::Tlsa(tlsa)))
        .collect();
    place(&mut entries, offset, tlsa);
    entries.extend(
        caa_records(answers)
            .into_iter()
            .map(|caa| Some(AnyRecord::Caa(caa))),
    );
    Ok(entries)
}

/// The result of a query of `kind` from its response `message`, as the resolver method gives it.
pub(crate) fn parse(kind: QueryKind, message: &Message) -> Result<Answer, Status> {
    let answers = message.answers.as_slice();
    Ok(match kind {
        QueryKind::A => Answer::Addresses(addresses(answers, false)?),
        QueryKind::Aaaa => Answer::Addresses(addresses(answers, true)?),
        QueryKind::Cname => match first_cname(answers) {
            Some(target) => Answer::Names(vec![target]),
            None if answers
                .iter()
                .any(|record| matches!(record.data, RData::A(_))) =>
            {
                Answer::Names(vec![name_text(message.queries[0].name())])
            }
            None => return Err(Status::NoData),
        },
        QueryKind::Ns => Answer::Names(names(answers, false)?),
        QueryKind::Ptr => Answer::Names(names(answers, true)?),
        QueryKind::Reverse => Answer::Names(names(answers, true).map_err(|_| Status::NotFound)?),
        QueryKind::Mx => Answer::Mx(mx_records(answers)),
        QueryKind::Txt => Answer::Txt(txt_records(answers)),
        QueryKind::Srv => Answer::Srv(srv_records(answers)),
        QueryKind::Naptr => Answer::Naptr(naptr_records(answers)?),
        QueryKind::Soa => Answer::Soa(soa_record(answers).ok_or(Status::BadResp)?),
        QueryKind::Caa => Answer::Caa(caa_records(answers)),
        QueryKind::Tlsa => {
            let mut entries = Vec::new();
            place(&mut entries, 0, tlsa_records(answers));
            Answer::Tlsa(entries)
        }
        QueryKind::Any => Answer::Any(any_records(answers)?),
    })
}
