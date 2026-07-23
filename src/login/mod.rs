//! The login window, run as `rsclient login` in a child process.
//!
//! Jagex's login page is a real web application — 2FA, social sign-in, bot checks — so it
//! has to be rendered by a real browser engine. This drives a WebKitGTK webview through
//! both OAuth legs, cancelling the two navigations that carry the results back, and hands
//! the resulting session to the caller.
//!
//! It lives in its own process because a GTK main loop and egui's `winit` loop cannot
//! share a thread. See the module docs in `main.rs`.

use anyhow::{Context, Result, anyhow, bail};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use wry::{WebViewBuilder, WebViewBuilderExtUnix};

use crate::auth::jwt::IdToken;
use crate::auth::{self, LoginFlow, Redirect, session, token};
use crate::store::{AccountSession, Session};

const WINDOW_WIDTH: f64 = 480.0;
const WINDOW_HEIGHT: f64 = 720.0;

/// Sent from the worker thread to the GTK main thread.
enum UserEvent {
    /// Move the webview on to the consent leg.
    Navigate(String),
    /// The flow finished, one way or the other.
    Finished(Box<Result<Session>>),
}

/// Runs the login window to completion.
pub fn run() -> Result<Session> {
    let mut flow = LoginFlow::new();
    let launcher_url = flow.launcher_url();

    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title("Sign in — rsclient")
        .with_inner_size(tao::dpi::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT))
        .build(&event_loop)
        .context("could not open the login window")?;

    // The navigation handler runs on the GTK main thread and must not block, so it only
    // forwards recognised redirects to the worker and returns immediately.
    let (redirect_tx, redirect_rx) = channel::<Redirect>();

    let builder = WebViewBuilder::new()
        .with_url(&launcher_url)
        .with_navigation_handler(move |url| match auth::classify_redirect(&url) {
            Some(redirect) => {
                // Returning false cancels the navigation, so the request is never sent:
                // the authorization code and id token stay in this process rather than
                // being handed to secure.runescape.com or to a local listener. It is also
                // why nothing here has to bind a port to catch the callback.
                let _ = redirect_tx.send(redirect);
                false
            }
            None => true,
        });

    // Build into the window's GTK container rather than from a raw window handle: the
    // WebKitGTK backend needs a real GTK widget, and the handle-based path is not
    // supported on Wayland at all.
    let vbox = window
        .default_vbox()
        .context("the login window has no GTK container to host the webview")?;
    let webview = builder
        .build_gtk(vbox)
        .context("could not create the login webview — is the webkit2gtk-4.1 package installed?")?;

    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        let outcome = drive_flow(&mut flow, &redirect_rx, &proxy);
        let _ = proxy.send_event(UserEvent::Finished(Box::new(outcome)));
    });

    // Stays `None` if the user closes the window before signing in.
    let mut result: Option<Result<Session>> = None;

    event_loop.run_return(|event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Navigate(url)) => {
                if let Err(e) = webview.load_url(&url) {
                    result = Some(Err(anyhow!("could not open the consent page: {e}")));
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Finished(outcome)) => {
                result = Some(*outcome);
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });

    result.unwrap_or_else(|| Err(anyhow!("sign-in was cancelled")))
}

/// Walks the OAuth legs, blocking on the redirects the webview forwards.
///
/// Runs on a worker thread: every step here does blocking network I/O, which must not
/// happen on the thread driving the webview.
fn drive_flow(
    flow: &mut LoginFlow,
    redirects: &Receiver<Redirect>,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<Session> {
    let client = http_client()?;

    // Leg 1: the user signs in, and we get an authorization code.
    let redirect = recv_redirect(redirects)?;
    let Redirect::Launcher { code, .. } = &redirect else {
        bail!("the consent step arrived before sign-in — aborting login");
    };
    let code = code.clone();
    flow.verify_state(&redirect)?;

    let tokens = token::exchange_code(&client, &code, flow.verifier())?;
    let claims = tokens.id_token.claims()?;

    // A legacy RuneScape account has no game session and no character list; it plays
    // using these tokens directly, so the consent leg does not apply.
    if claims.is_legacy_runescape() {
        let display_name = session::fetch_profile_display_name(&client, &tokens.id_token)?;
        return Ok(Session {
            tokens,
            account_name: Some(display_name.clone()),
            account: AccountSession::Runescape { display_name },
            selected_character: None,
        });
    }

    // Leg 2: consent, which yields the id token the game session is built from. It has to
    // load in this same webview — it relies on the login cookies just set.
    flow.set_sub(&claims.sub);
    proxy
        .send_event(UserEvent::Navigate(flow.consent_url(&tokens.id_token)))
        .map_err(|_| anyhow!("the login window closed before consent could be requested"))?;

    let redirect = recv_redirect(redirects)?;
    let Redirect::Consent { id_token, .. } = &redirect else {
        bail!("signed in twice without consenting — aborting login");
    };
    let id_token = IdToken::new(id_token.clone());
    flow.verify_state(&redirect)?;
    let consent_claims = flow.verify_consent_token(&id_token)?;

    // Leg 3: exchange it for a session id and the character list.
    let session_id = session::create_session(&client, &id_token)?;
    let accounts = session::fetch_accounts(&client, &session_id)?;
    if accounts.is_empty() {
        bail!("this Jagex account has no characters — create one on the website first");
    }

    Ok(Session {
        tokens,
        account_name: consent_claims.nickname.clone(),
        account: AccountSession::Jagex {
            session_id,
            accounts,
        },
        selected_character: None,
    })
}

fn recv_redirect(redirects: &Receiver<Redirect>) -> Result<Redirect> {
    redirects
        .recv()
        .context("the login window was closed before sign-in finished")
}

/// An HTTP client for the Jagex auth endpoints.
///
/// Redirects are not followed: in this flow every redirect is a result to interpret, not
/// a hop to chase.
pub fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .context("could not create an HTTP client")
}
