# Changelog

## Unreleased

### Sign in with your login provider

Sign in to Soulbeet through Pocket ID, Authelia, Authentik, Keycloak or another OpenID Connect provider, next to your password.

- **A button on the login page**: Once an admin sets it up, the login page has a button to sign in through your provider.
- **Your account stays yours**: A first sign-in never takes an existing account, so sign in with your password once and link single sign-on in Settings > Users.
- **Navidrome**: An account made through single sign-on gets Navidrome with Connect Navidrome in Settings > Users.
- **For admins**: Single sign-on creates accounts by default only with `REGISTRATION=open`; see Single Sign-On in the README.

[How to set it up](https://github.com/terry90/soulbeet#single-sign-on-openid-connect)

## 0.9.0 - 2026-11-19

### Ask your AI assistant for music

Connect Claude, ChatGPT or another AI assistant to Soulbeet, then ask for music in a chat.

- **Ask in plain words**: Ask for an album or a whole playlist, and it downloads what you are missing and builds the playlist in Navidrome.
- **It keeps going**: Downloads carry on after you close the chat.
- **Deleting is off**: An assistant cannot delete music until you allow it in Settings > Users > Connected apps.
- **You were signed out**: This update signs everyone out once, so sign in again.
- **For admins**: Only the `admin` account keeps admin rights; see Upgrading in the README.

[How to set it up](https://github.com/terry90/soulbeet#mcp-server)
