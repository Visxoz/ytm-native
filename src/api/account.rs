//! Signing in and out, and what YouTube says about the account.

use rustypipe::client::ClientType;

use super::{Api, Result, err};

impl Api {
    pub async fn sign_in(&self, cookie: &str) -> Result<()> {
        let cookie = cookie.trim();
        if cookie.contains('\t') {
            self.rp.user_auth_set_cookie_txt(cookie).await.map_err(err)
        } else {
            let cookie = cookie.strip_prefix("Cookie:").unwrap_or(cookie).trim();
            self.rp.user_auth_set_cookie(cookie).await.map_err(err)
        }
    }

    pub async fn sign_out(&self) {
        let _ = self.rp.user_auth_remove_cookie().await;
    }

    /// The signed-in account's profile picture, from YouTube Music's
    /// account menu.
    pub async fn account_photo(&self) -> Result<Option<String>> {
        let json = self
            .q()
            .authenticated()
            .raw(
                ClientType::DesktopMusic,
                "account/account_menu",
                &serde_json::json!({}),
            )
            .await
            .map_err(err)?;
        Ok(account_photo(&json))
    }

    /// Whether a sign-in cookie is stored, without asking YouTube.
    pub fn has_cookie(&self) -> bool {
        self.q().auth_enabled(ClientType::DesktopMusic)
    }

    /// Checks the stored cookie with YouTube, which costs a page load.
    /// `None` when YouTube could not be asked.
    pub async fn cookie_valid(&self) -> Option<bool> {
        match self.rp.user_auth_check_cookie().await {
            Ok(()) => Some(true),
            Err(rustypipe::error::Error::Auth(_)) => Some(false),
            Err(e) => {
                log::warn!("could not check the sign-in: {e}");
                None
            }
        }
    }
}

/// The largest profile picture in an account menu response.
fn account_photo(json: &str) -> Option<String> {
    let menu: serde_json::Value = serde_json::from_str(json).ok()?;
    let header = &menu["actions"][0]["openPopupAction"]["popup"]["multiPageMenuRenderer"]["header"]
        ["activeAccountHeaderRenderer"];
    header["accountPhoto"]["thumbnails"]
        .as_array()?
        .iter()
        .max_by_key(|t| t["width"].as_u64().unwrap_or(0))?["url"]
        .as_str()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_account_photo() {
        let menu = r#"{"actions":[{"openPopupAction":{"popup":{"multiPageMenuRenderer":{"header":
            {"activeAccountHeaderRenderer":{"accountName":{"runs":[{"text":"Alex"}]},
            "accountPhoto":{"thumbnails":[
                {"url":"https://yt3.ggpht.com/a=s88","width":88,"height":88},
                {"url":"https://yt3.ggpht.com/a=s176","width":176,"height":176}]}}}}}}}]}"#;
        assert_eq!(
            account_photo(menu).as_deref(),
            Some("https://yt3.ggpht.com/a=s176")
        );
        assert_eq!(account_photo("{}"), None);
        assert_eq!(account_photo("not json"), None);
    }
}
