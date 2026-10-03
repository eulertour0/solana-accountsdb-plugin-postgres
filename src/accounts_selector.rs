use {log::*, std::collections::HashSet};

#[derive(Debug)]
pub(crate) struct AccountsSelector {
    pub accounts: HashSet<Vec<u8>>,
    pub owners: HashSet<Vec<u8>>,
    pub select_all_accounts: bool,
}

impl AccountsSelector {
    pub fn default() -> Self {
        AccountsSelector {
            accounts: HashSet::default(),
            owners: HashSet::default(),
            select_all_accounts: true,
        }
    }

    pub fn new(accounts: &[String], owners: &[String]) -> Result<Self, String> {
        info!(
            "Creating AccountsSelector from accounts: {:?}, owners: {:?}",
            accounts, owners
        );

        let select_all_accounts = accounts.iter().any(|key| key == "*");
        if select_all_accounts {
            return Ok(AccountsSelector {
                accounts: HashSet::default(),
                owners: HashSet::default(),
                select_all_accounts,
            });
        }
        let decode = |keys: &[String]| {
            keys.iter()
                .map(|key| {
                    bs58::decode(key)
                        .into_vec()
                        .map_err(|err| format!("Invalid accounts_selector key {key:?}: {err}"))
                })
                .collect::<Result<HashSet<_>, _>>()
        };
        Ok(AccountsSelector {
            accounts: decode(accounts)?,
            owners: decode(owners)?,
            select_all_accounts,
        })
    }

    pub fn is_account_selected(&self, account: &[u8], owner: &[u8]) -> bool {
        self.select_all_accounts || self.accounts.contains(account) || self.owners.contains(owner)
    }

    /// Check if any account is of interested at all
    pub fn is_enabled(&self) -> bool {
        self.select_all_accounts || !self.accounts.is_empty() || !self.owners.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn test_create_accounts_selector() {
        AccountsSelector::new(
            &["9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin".to_string()],
            &[],
        )
        .unwrap();

        AccountsSelector::new(
            &[],
            &["9xQeWvG816bUx9EPjHmaT23yvVM2ZWbrrpZb9PusVFin".to_string()],
        )
        .unwrap();
    }
}
