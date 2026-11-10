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

    /// The signed-in account's profile picture. YouTube's guide has the
    /// current one, while the account menu has been seen to keep serving the
    /// one before a change, so the menu is asked only when the guide has none.
    pub async fn account_photo(&self) -> Result<Option<String>> {
        let ask = async |client, endpoint: &str| {
            self.q()
                .authenticated()
                .raw(client, endpoint, &serde_json::json!({}))
                .await
        };
        match ask(ClientType::Desktop, "guide").await {
            Ok(guide) => match account_photo(&guide, GUIDE_PHOTO) {
                Some(url) => return Ok(Some(url)),
                None => log::debug!("the guide has no account picture"),
            },
            Err(e) => log::debug!("the guide failed: {e}"),
        }
        let menu = ask(ClientType::DesktopMusic, "account/account_menu")
            .await
            .map_err(err)?;
        Ok(account_photo(&menu, MENU_PHOTO))
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

/// Where the profile pictures sit in the guide and account menu responses.
const GUIDE_PHOTO: &str = "/userProfile/accountPhoto/thumbnails";
const MENU_PHOTO: &str = "/actions/0/openPopupAction/popup/multiPageMenuRenderer/header/activeAccountHeaderRenderer/accountPhoto/thumbnails";

/// The largest profile picture at `pointer` in a response, as a JPEG: an
/// animated picture otherwise comes as a GIF, which artwork isn't decoded from.
fn account_photo(json: &str, pointer: &str) -> Option<String> {
    let response: serde_json::Value = serde_json::from_str(json).ok()?;
    let url = response
        .pointer(pointer)?
        .as_array()?
        .iter()
        .max_by_key(|t| t["width"].as_u64().unwrap_or(0))?["url"]
        .as_str()?;
    Some(if url.contains('=') && !url.ends_with("-rj") {
        format!("{url}-rj")
    } else {
        url.to_owned()
    })
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
            account_photo(menu, MENU_PHOTO).as_deref(),
            Some("https://yt3.ggpht.com/a=s176-rj")
        );
        let guide = r#"{"items":[],"userProfile":{"accountPhoto":{"thumbnails":[
            {"url":"https://yt3.ggpht.com/yti/b=s88-c-k-no-rj"}]}}}"#;
        assert_eq!(
            account_photo(guide, GUIDE_PHOTO).as_deref(),
            Some("https://yt3.ggpht.com/yti/b=s88-c-k-no-rj")
        );
        assert_eq!(account_photo(menu, GUIDE_PHOTO), None);
        assert_eq!(account_photo("{}", MENU_PHOTO), None);
        assert_eq!(account_photo("not json", MENU_PHOTO), None);
    }
}
