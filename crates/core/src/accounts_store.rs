//! profiles/accounts.json, written by the config dialog's Account Config tab. Shared so the main app can read account membership for Display Regions' account cells without pulling in config.exe's webui code.

use std::collections::HashMap;
use std::io::Read;

use serde::{Deserialize, Serialize};

pub const ACCOUNTS_FILE: &str = "profiles/accounts.json";
pub const MAX_FILE_SIZE: usize = 1024 * 1024;

/// On-disk shape of accounts.json. `userIds` are EVE's own account IDs (from core_user_<id>.dat), linked so scan suggestions can map onto a user-named account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountsFile {
    pub version: u32,
    pub accounts: Vec<Account>,
    pub characters: Vec<CharacterLink>,
}

impl Default for AccountsFile {
    fn default() -> Self {
        Self { version: 1, accounts: Vec::new(), characters: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub user_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterLink {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub last_seen: Option<i64>,
}

/// Lower-cased character name -> account id, for every named, linked character.
pub type Membership = HashMap<String, String>;

pub fn membership_from_file(file: &AccountsFile) -> Membership {
    let mut map = Membership::new();
    for c in &file.characters {
        let Some(name) = c.name.as_deref() else { continue };
        let Some(account) = c.account_id.as_deref() else { continue };
        if name.is_empty() || account.is_empty() {
            continue;
        }
        map.insert(name.to_ascii_lowercase(), account.to_owned());
    }
    map
}

/// Reads accounts.json from the working directory, refusing anything over MAX_FILE_SIZE.
pub fn read_accounts_bytes() -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(ACCOUNTS_FILE)?;
    let mut content = Vec::new();
    file.take(MAX_FILE_SIZE as u64 + 1).read_to_end(&mut content)?;
    if content.len() > MAX_FILE_SIZE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "StreamTooLong"));
    }
    Ok(content)
}

/// Parses accounts.json content; unknown fields are ignored.
pub fn parse_accounts_file(content: &[u8]) -> serde_json::Result<AccountsFile> {
    serde_json::from_slice(content)
}

/// Reads accounts.json from the working directory; an empty map if it's missing or malformed (account cells then just match nobody).
pub fn load_membership() -> Membership {
    let Ok(content) = read_accounts_bytes() else { return Membership::new() };
    let Ok(file) = parse_accounts_file(&content) else { return Membership::new() };
    membership_from_file(&file)
}

pub fn account_of<'a>(map: &'a Membership, name: &str) -> Option<&'a str> {
    if name.len() > 128 {
        return None;
    }
    map.get(&name.to_ascii_lowercase()).map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(id: &str, name: Option<&str>, account_id: Option<&str>) -> CharacterLink {
        CharacterLink { id: id.into(), name: name.map(Into::into), account_id: account_id.map(Into::into), last_seen: None }
    }

    #[test]
    fn membership_maps_names_case_insensitively_and_skips_unlinked_characters() {
        let file = AccountsFile {
            characters: vec![
                link("1", Some("FC Zoetrope"), Some("acc_1")),
                link("2", Some("Alt Two"), None),
                link("3", None, Some("acc_1")),
            ],
            ..Default::default()
        };
        let map = membership_from_file(&file);
        assert_eq!(account_of(&map, "fc zoetrope"), Some("acc_1"));
        assert_eq!(account_of(&map, "Alt Two"), None);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn account_of_rejects_names_longer_than_the_lookup_buffer() {
        let long = "a".repeat(129);
        let file = AccountsFile { characters: vec![link("1", Some(&long), Some("acc"))], ..Default::default() };
        let map = membership_from_file(&file);
        assert_eq!(account_of(&map, &long), None);
        assert_eq!(account_of(&map, &"A".repeat(128)), None);
    }

    #[test]
    fn parses_with_defaults_and_ignores_unknown_fields() {
        let json = br#"{"extra":true,"accounts":[{"id":"a","name":"Main","color":"red"}],"characters":[{"id":"9","lastSeen":5}]}"#;
        let file = parse_accounts_file(json).unwrap();
        assert_eq!(file.version, 1);
        assert_eq!(file.accounts[0].user_ids, Vec::<String>::new());
        assert_eq!(file.characters[0].last_seen, Some(5));
        assert_eq!(file.characters[0].name, None);
        assert_eq!(parse_accounts_file(b"{}").unwrap(), AccountsFile::default());
    }

    #[test]
    fn rejects_missing_required_fields() {
        assert!(parse_accounts_file(br#"{"accounts":[{"id":"a"}]}"#).is_err());
        assert!(parse_accounts_file(br#"{"characters":[{"name":"x"}]}"#).is_err());
        assert!(parse_accounts_file(br#"{"accounts":null}"#).is_err());
    }

    #[test]
    fn serializes_in_the_zig_field_order_with_explicit_nulls() {
        let file = AccountsFile {
            version: 1,
            accounts: vec![Account { id: "a".into(), name: "Main".into(), user_ids: vec!["77".into()] }],
            characters: vec![link("9", Some("Pilot"), None)],
        };
        assert_eq!(
            serde_json::to_string(&file).unwrap(),
            r#"{"version":1,"accounts":[{"id":"a","name":"Main","userIds":["77"]}],"characters":[{"id":"9","name":"Pilot","accountId":null,"lastSeen":null}]}"#
        );
    }
}
