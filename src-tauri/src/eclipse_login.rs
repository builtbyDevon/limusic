//! Our own Eclipse login, stored in our Keychain item (never Eclipse's private group).
use tauri::{AppHandle, Manager};

#[cfg(target_os = "macos")]
const SERVICE: &str = "com.limusic.desktop.eclipse";

#[cfg(target_os = "macos")]
pub fn credentials() -> Option<(String, String)> {
    let bytes = security_framework::passwords::get_generic_password(SERVICE, "cloud").ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(not(target_os = "macos"))]
pub fn credentials() -> Option<(String, String)> { None }

#[tauri::command]
pub fn connect_eclipse(app: AppHandle) -> Result<(), String> {
    #[cfg(not(target_os = "macos"))]
    { let _ = app; return Err("Use your signed-in Eclipse app on this platform.".into()); }
    #[cfg(target_os = "macos")]
    {
        use tauri::{Emitter, WebviewUrl, WebviewWindowBuilder};
        if let Some(window) = app.get_webview_window("eclipse-login") {
            let _ = window.set_focus();
            return Ok(());
        }
        let nonce = format!("{:032x}", rand::random::<u128>());
        let script = format!(r#"
            if (location.origin === 'https://eclipsemusic.app') {{
                const timer = setInterval(() => {{
                    const token = localStorage.getItem('eclipse_token');
                    if (!token) return;
                    if (location.pathname !== '/account' || location.hash !== '#support') {{
                        location.replace('/account#support'); return;
                    }}
                    const addon = document.getElementById('sup-url')?.value;
                    if (!addon) return;
                    clearInterval(timer);
                    location.href = 'limusiceclipse://connected?nonce={nonce}&token=' +
                        encodeURIComponent(token) + '&addon=' + encodeURIComponent(addon);
                }}, 750);
                setTimeout(() => clearInterval(timer), 600000);
            }}
        "#);
        let callback_app = app.clone();
        let builder = WebviewWindowBuilder::new(&app, "eclipse-login",
            WebviewUrl::External("https://eclipsemusic.app/sign-in".parse().unwrap()))
            .title("Connect Eclipse to LiMusic")
            .inner_size(560.0, 760.0)
            .initialization_script(&script)
            .on_navigation(move |url| {
                // Eclipse omits Google's account chooser. Keep its OAuth parameters intact,
                // but require a choice when this window reaches the authorization endpoint.
                if url.scheme() == "https" && url.host_str() == Some("accounts.google.com")
                    && url.path() == "/o/oauth2/v2/auth"
                    && !url.query_pairs().any(|(key, value)| key == "prompt" && value.split_whitespace().any(|p| p == "select_account"))
                {
                    let mut chooser = url.clone();
                    let mut pairs: Vec<(String, String)> = chooser.query_pairs().into_owned().collect();
                    if let Some((_, prompt)) = pairs.iter_mut().find(|(key, _)| key == "prompt") {
                        prompt.push_str(" select_account");
                    } else { pairs.push(("prompt".into(), "select_account".into())); }
                    chooser.query_pairs_mut().clear().extend_pairs(pairs);
                    let app = callback_app.clone();
                    let dispatch = app.clone();
                    let _ = dispatch.run_on_main_thread(move || {
                        if let Some(w) = app.get_webview_window("eclipse-login") { let _ = w.navigate(chooser); }
                    });
                    return false;
                }
                if url.scheme() != "limusiceclipse" { return true; }
                let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
                if url.host_str() != Some("connected") || pairs.get("nonce") != Some(&nonce) { return false; }
                let Some(token) = pairs.get("token").filter(|t| !t.is_empty() && t.len() < 16384).cloned() else { return false; };
                let Some(addon) = pairs.get("addon").cloned() else { return false; };
                let app = callback_app.clone();
                tauri::async_runtime::spawn(async move {
                    // Verify authorization against the actual addon before accepting the session.
                    let result = verify_and_save(token, addon).await;
                    match result {
                        Ok(()) => {
                            let state = app.state::<std::sync::Arc<crate::state::AppState>>();
                            state.eclipse.clear_authorization_cache().await;
                            let _ = app.emit("eclipse-connected", ());
                            if let Some(w) = app.get_webview_window("eclipse-login") { let _ = w.close(); }
                        }
                        Err(message) => {
                            let _ = app.emit("eclipse-login-error", message);
                        }
                    }
                });
                false
            });
        builder.build().map_err(|_| "Could not open Eclipse sign-in.".to_owned())?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
async fn verify_and_save(token: String, addon: String) -> Result<(), String> {
    let mut url = reqwest::Url::parse(&addon).map_err(|_| "Invalid Eclipse addon URL.")?;
    if url.scheme() != "https" || url.host_str() != Some("api.eclipsemusic.app")
        || url.port().is_some() || !url.username().is_empty() || url.password().is_some()
        || !url.path().starts_with("/addon/") || !url.path().ends_with("/music/manifest.json")
    { return Err("Unexpected Eclipse addon URL.".into()); }
    let path = url.path().trim_end_matches("manifest.json").to_owned() + "search";
    url.set_path(&path);
    url.set_query(Some("q=Daft%20Punk&type=tracks"));
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20)).build().map_err(|_| "Could not connect to Eclipse.")?;
    let response = client.get(url).bearer_auth(&token).header("X-Client-Platform", "macos")
        .send().await.map_err(|_| "Eclipse could not be reached. Please try connecting again.")?;
    if !response.status().is_success() {
        return Err(format!("Eclipse refused addon access ({}). Please check Cloud access in your Eclipse account.", response.status().as_u16()));
    }
    let bytes = serde_json::to_vec(&(token, addon)).map_err(|_| "Could not save Eclipse login.")?;
    tokio::task::spawn_blocking(move || security_framework::passwords::set_generic_password(SERVICE, "cloud", &bytes))
        .await.map_err(|_| "Could not save Eclipse login.")?
        .map_err(|_| "Could not save Eclipse login in Keychain.".into())
}
