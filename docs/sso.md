# Single sign-on (OpenID Connect)

This guide sets up sign-in through an OpenID Connect provider and explains how those sign-ins map to Soulbeet accounts.

## What it does

Soulbeet can sign users in through an OpenID Connect provider such as [Pocket ID](https://pocket-id.org), Authelia, Authentik or Keycloak. The login page then shows a **Sign in with ...** button next to the password form, which keeps working.

Read the [warning about names](#warning-the-same-name-joins-the-same-account) before you turn it on.

## Setup

1.  At your provider, create a client:
    -   The provider sends users back to `/api/auth/oidc/callback` at the address they started from, so register that path for each address people sign in from as a redirect URI, for example `https://soulbeet.example.com/api/auth/oidc/callback` and `http://192.168.1.10:9765/api/auth/oidc/callback`.
    -   Register each address exactly, never a wildcard or pattern: Soulbeet builds the address from the request, so a pattern would let a forged request send a sign-in to another site.
    -   When Soulbeet can be reached from the internet, set `PUBLIC_URL` or `OIDC_REDIRECT_URL`. With `PUBLIC_URL` or `OIDC_REDIRECT_URL` set, only that address is used: a sign-in started at another address moves there first, and single sign-on is linked in Settings there.
    -   Soulbeet always sends PKCE, so the client can be public (no secret) if the provider allows it.
    -   Restrict who may sign in at the provider (Pocket ID: allowed user groups on the client).
2.  Set the variables, for example:
    ```yaml
    environment:
      - OIDC_ISSUER_URL=https://id.example.com
      - OIDC_CLIENT_ID=<client id>
      - OIDC_CLIENT_SECRET=<client secret>
      - OIDC_PROVIDER_NAME=Pocket ID
    ```
3.  Restart. A configuration error leaves single sign-on off. It is logged at startup and shown in Settings > Config > Security Checks.

## Settings

| Variable | Description | Default |
|----------|-------------|---------|
| `OIDC_ISSUER_URL` | Issuer of your OpenID Connect provider. Setting it turns on single sign-on | |
| `OIDC_CLIENT_ID` | Client ID registered at the provider | |
| `OIDC_CLIENT_SECRET` | Client secret. Leave unset for a public client (PKCE only) | |
| `OIDC_REDIRECT_URL` | Optional. One callback for every sign-in, `https://<your soulbeet>/api/auth/oidc/callback`. Without it, the provider sends users back to the address they started at (`PUBLIC_URL` when set) | |
| `OIDC_PROVIDER_NAME` | Name on the login button ("Sign in with ...") | `SSO` |
| `OIDC_SCOPES` | Scopes to request (`openid` is always added) | `openid profile email` |
| `OIDC_USERNAME_CLAIM` | ID token claim used as the Soulbeet username | `preferred_username` |
| `OIDC_AUTO_REGISTER` | Create an account on first sign-in when none has that username | `true` with `REGISTRATION=open`, else `false` |
| `OIDC_LINK_BY_USERNAME` | On a first sign-in, link the existing account of that username. Set `false` to refuse such a sign-in instead. Read the [warning](#warning-the-same-name-joins-the-same-account) | `true` |

`PUBLIC_URL`, `REGISTRATION` and the other variables are in [configuration.md](configuration.md).

## How accounts are matched

-   An account is tied to the provider's user (its issuer and `sub`), so renaming the user at the provider does not change accounts.
-   **The same name joins the same account**: when an account already has the name `OIDC_USERNAME_CLAIM` gives (ignoring letter case, like every sign-in) and no single sign-on link yet, a first sign-in joins that account and links it, which signs the account out of its other sessions. An account linked to another identity is never joined. Read the [warning](#warning-the-same-name-joins-the-same-account) on when to turn this off.
-   **No account of that name**: a first sign-in creates one as a plain user when `OIDC_AUTO_REGISTER` is on. `OIDC_AUTO_REGISTER` is on by default only with `REGISTRATION=open`.
    -   With Navidrome, an account that single sign-on creates takes that name: if the Navidrome user of that name is someone else, they cannot sign in until an admin deletes that account.
    -   When `OIDC_AUTO_REGISTER` is off, the sign-in is refused and the user gets an account the way they would without single sign-on: with `REGISTRATION=navidrome` by signing in once with their Navidrome password, with `REGISTRATION=admin` from an admin in Settings > Users. Their next single sign-on then joins that account, or, with `OIDC_LINK_BY_USERNAME=false`, they sign in with that password and link single sign-on in Settings.
-   **Email as the username**: with `OIDC_USERNAME_CLAIM=email`, a sign-in is accepted only when the provider says the address is verified (`email_verified` is `true`). A missing or any other value is refused.

## Warning: the same name joins the same account

**By default, whoever holds a name at the provider gets the Soulbeet account of that name, admin accounts included.** Unlinking an account taken this way, or an admin setting its password (which removes the link), only helps once `OIDC_LINK_BY_USERNAME` is `false` or the name at the provider is changed: the next sign-in under that name links it again.

### When the default is fine

The default is fine for a family, friends or local instance where the admin runs the provider too, creates the accounts there, and people cannot pick or change their own username at the provider. Some providers let users edit their own username unless the admin turns that off (Pocket ID does by default), so check that setting before you keep the default.

With `REGISTRATION=navidrome`, the admin also creates the Navidrome accounts: give them the same names as at the provider. Then the same name means the same person, and existing Soulbeet users just click **Sign in with ...**.

### When to turn it off

Set `OIDC_LINK_BY_USERNAME=false` if any of these is true:

-   People can sign up at the provider by themselves, or change their username there (or the claim `OIDC_USERNAME_CLAIM` names).
-   The provider is shared with people who should not all get into Soulbeet, such as a company or public provider.
-   Soulbeet runs with `REGISTRATION=open`, or strangers can create accounts in it some other way. Someone can register a name with a password first; the real person's first single sign-on then joins that account, and the stranger keeps the password.
-   Soulbeet runs with `REGISTRATION=navidrome`, and a Navidrome user may have the same name as a different person at the provider. A Navidrome user who signed in to Soulbeet once has an account, and the first single sign-on under that name joins it, with that user's Navidrome library, ratings and playlists.
-   Someone at the provider holds the name of a Soulbeet admin (the seeded one is `admin`) without being that admin. They would sign in as that admin.

With it off, a first sign-in under the name of an existing account is refused. The owner signs in with their password and links single sign-on in Settings > Users.

## Linking and unlinking

-   **Linking**: signed in, a user can also link single sign-on in Settings > Users, for example when their name at the provider is not their Soulbeet name.
    -   They enter their current password first, the one the login form takes (their Navidrome password for an account that has only that), so a session left open is not enough to link someone else's identity to the account.
    -   Linking sends them to the provider and back. Soulbeet asks the provider to make them sign in there again, and Settings then shows the name of the identity linked, so they can check that it is theirs and not a provider session someone else left open in that browser.
    -   An identity is linked to one account only, and an account to one identity.
-   **Changing their own password** asks for the current one the same way, so such a session cannot set a new one first. A user who changes their own password keeps their link.
-   **Unlinking**: a user can unlink their own account in Settings > Users while it keeps another way in (a password of its own, or a stored Navidrome password).
-   **Admins** see in Settings > Users which accounts use single sign-on, with the name of the identity at the provider, and which have no password, and can unlink any of them. An admin who sets the password of another account also removes its link, so whoever held that identity is out (see the [warning](#warning-the-same-name-joins-the-same-account) for a link made by name).
-   **An account with no password at all** gets one from an admin, which also removes its single sign-on link, so the user's next single sign-on joins it again by name, or they link it in Settings.
-   Linking, unlinking or a new password signs the account out everywhere else.

## Navidrome

An account created through single sign-on has no password and no Navidrome password yet, and a banner says so. A Navidrome login with the same username does not join it, because the provider and Navidrome may give that name to different people. Instead, the user enters their Navidrome password under **Connect Navidrome** in Settings > Users. Soulbeet checks it with Navidrome for the account's own username and stores it. Any account can do this, also to store a Navidrome password that changed.

## Notes

-   **Signing out** of Soulbeet does not sign you out of the provider. On a shared computer, sign out at the provider too, or the next person to click **Sign in with ...** gets your account.
-   **MCP clients**: signing in from an MCP client's authorization page goes back to it afterwards, as a password sign-in does, also after a failed single sign-on: the error page keeps the request. See [mcp.md](mcp.md).
-   **Changing provider**: unlink each account in Settings > Users, set a password on the accounts marked as having none, and have users sign in with the new provider, which joins accounts by name, or link it in Settings.
