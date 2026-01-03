use std::future::Future;
use std::pin::Pin;

use api::login;
use dioxus::prelude::*;
use ui::Login;

use crate::auth::use_auth;
use crate::Route;

#[component]
pub fn LoginPage() -> Element {
    let navigator = use_navigator();
    let mut auth = use_auth();

    let login = use_callback(move |(username, password): (String, String)| 
        -> Pin<Box<dyn Future<Output = Result<(), String>>>> 
    {
        Box::pin(async move {
            match login(username, password).await {
                Ok(response) => {
                    auth.login(response); 
                    navigator.push(Route::SearchPage {});
                    Ok(())
                }
                _ => Err("Invalid username or password".to_string()),
            }
        })
    });


    rsx! {
        Login { login }
    }
}
