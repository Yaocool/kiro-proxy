//! Chromium automation used only by the full SSO build.

use anyhow::{Context, Result};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Owns Chromium and the two background drivers needed during login.
pub struct BrowserSession {
    browser: Browser,
    handler: JoinHandle<()>,
    injector: JoinHandle<()>,
    cancel: CancellationToken,
}

impl BrowserSession {
    pub async fn launch(
        authorize_url: &str,
        email: &str,
        password: &str,
        headful: bool,
    ) -> Result<Self> {
        let mut builder = BrowserConfig::builder()
            .incognito()
            .window_size(1100, 800)
            .launch_timeout(std::time::Duration::from_secs(20));
        if std::env::var("KAM_CHROMIUM_NO_SANDBOX").as_deref() == Ok("1") {
            builder = builder.no_sandbox();
        }
        if headful {
            builder = builder.with_head();
        }
        let config = builder
            .build()
            .map_err(|error| anyhow::anyhow!("invalid Chromium configuration: {error}"))?;
        let (mut browser, mut events) = Browser::launch(config)
            .await
            .context("unable to launch Chromium; install Chrome/Chromium or use a full image")?;
        let handler = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if event.is_err() {
                    break;
                }
            }
        });
        browser.start_incognito_context().await?;
        let page = browser.new_page(authorize_url).await?;
        let cancel = CancellationToken::new();
        let injector = spawn_injector(page, email, password, cancel.clone())?;
        Ok(Self {
            browser,
            handler,
            injector,
            cancel,
        })
    }

    pub async fn close(&mut self) {
        self.cancel.cancel();
        self.injector.abort();
        let _ = self.browser.close().await;
        self.handler.abort();
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.injector.abort();
        self.handler.abort();
    }
}

fn spawn_injector(
    page: Page,
    email: &str,
    password: &str,
    cancel: CancellationToken,
) -> Result<JoinHandle<()>> {
    let email = serde_json::to_string(email)?;
    let password = serde_json::to_string(password)?;
    let script = automation_script(&email, &password);
    Ok(tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(600));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let _ = page.evaluate(script.clone()).await;
                }
            }
        }
    }))
}

fn automation_script(email: &str, password: &str) -> String {
    format!(
        r#"(() => {{
          const email = {email};
          const password = {password};
          const visible = el => !!el && !el.disabled && el.offsetParent !== null;
          const setValue = (el, value) => {{
            if (!visible(el) || el.value) return false;
            const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
            setter.call(el, value);
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return true;
          }};
          const first = selectors => selectors.map(s => document.querySelector(s)).find(visible);
          const user = first(['input[name="loginfmt"]','input[name="username"]','input[type="email"]','input[name="email"]','#i0116','#username','#email','input[autocomplete="username"]','input[autocomplete="email"]']);
          if (setValue(user, email)) {{
            const next = first(['#idSIButton9','button[type="submit"]','input[type="submit"]']);
            if (next) next.click();
            return 'username';
          }}
          const pass = first(['input[name="passwd"]','input[type="password"]','input[name="password"]','#i0118','#passwordInput','#password']);
          if (setValue(pass, password)) {{
            const submit = first(['#idSIButton9','button[type="submit"]','input[type="submit"]']);
            if (submit) submit.click();
            return 'password';
          }}
          const no = first(['#idBtn_Back']);
          if (no && /stay signed in/i.test(document.body.innerText)) {{ no.click(); return 'kmsi'; }}
          const allow = first(['button[name="allow"]','[data-testid="allow-button"]','[data-testid="allow-access-button"]','button[value="allow"]','#allow-button','button.awsui-button--primary','#cli_login_button','button[data-analytics="allowButton"]']);
          if (allow) {{ allow.click(); return 'allow'; }}
          const textAllow = [...document.querySelectorAll('button,input[type="submit"]')]
            .find(el => visible(el) && /^(allow|authorize|allow access|continue|confirm|允许|确认授权)/i.test((el.innerText || el.value || '').trim()));
          if (textAllow) {{ textAllow.click(); return 'allow-text'; }}
          return 'waiting';
        }})()"#
    )
}
