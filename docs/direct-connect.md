# Direct Connect

The Direct Connect download source searches the Direct Connect hubs an AirDC++ Web Client is connected to, and has AirDC++ download the files. You run AirDC++ next to Soulbeet, as you run slskd; Soulbeet talks to it over its Web API. It is off until an admin turns it on in Settings > Config > Connections.

## What it offers

Direct Connect hubs are communities of their own, each with its own rules. Many are old NMDC hubs with thousands of users and petabytes shared; what they hold depends on who is online. Soulbeet ships no hub list: you pick the hubs and follow their rules.

A file is offered only when all of these hold:

- it is an audio file Soulbeet imports, with its size and its TTH (the Tiger tree hash AirDC++ checks the bytes against);
- its title matches a track of what you searched for, scored as Soulseek files are, on one scale, so auto-download picks between the two by the same rule;
- its peer can send to your AirDC++: when AirDC++ runs in passive mode, peers that are passive too are left out, since two passive clients cannot connect to each other.

For an album, Soulbeet reads the file lists of the 4 best named folders (at most 2 per nick), so it knows which tracks a folder has beyond the few files the search returned (hubs send at most 5 results per user). A folder whose list does not load in 20 seconds is offered with the files the search returned only, and seldom scores high enough to be picked by itself.

NMDC search answers carry no length and no bitrate: a lossy file's bitrate is estimated from its size and the track's length and shown as `~320k`. A file's length is not checked before it downloads.

## How a search runs

Auto-download searches Direct Connect next to Soulseek. The manual "Search sources" view and the MCP tools search Soulseek only.

1. One Direct Connect search at a time for the whole install, at least 10 seconds after the previous one, or AirDC++'s own minimum search interval when that is longer (hubs often ask for more; set it in AirDC++). AirDC++ sends it to every connected hub. A search auto-download gives up on (after 3 minutes) is stopped and never sent.
2. The text is the artist and the album (or the track). Words with accented Latin letters are left out, and edition brackets such as `(Deluxe Edition)`, since hub searches match nothing with them. A text shorter than 5 characters is refused.
3. Results are read for 20 seconds, then the best folders' file lists are loaded, at most 2 at once for the install. A file list someone already opened in AirDC++ is left alone, and so is one Soulbeet opened that its user moved to another folder meanwhile.

## How a download runs

Each file is one file bundle in AirDC++'s queue, saved into the batch folder under the name Soulbeet looks for. A name already taken there lands as a copy with a time stamp in its name, as slskd does; nothing is replaced. While a file downloads, AirDC++ writes it as `<name>.dctmp` next to where it lands; Soulbeet never imports such a file.

A file counts as finished once AirDC++ has checked it against its TTH. Bytes that do not match fail it with `transfer_checksum_mismatch`.

Each file is recorded in Soulbeet's database before AirDC++ is asked to queue it, so after a restart of Soulbeet, or of AirDC++, its downloads are found again. AirDC++ itself never gives up on a file it cannot get. Soulbeet does: a file that got no byte for 5 minutes while AirDC++ only failed to connect to its peer fails with `transfer_connection_failed` (`peer_offline` when the peer was offline the whole time), and Soulbeet removes it from AirDC++'s queue. After a minute without a byte AirDC++ is asked once to look for other peers with the same file; these searches wait until no Direct Connect search of a download waits. A wait in a peer's slot queue ("No slots available", with or without a place) is left to the download monitor's limit for remote queues, and a file AirDC++ holds back for its own download slots or speed limit waits as long as it takes; neither counts toward the 5 minutes, nor does a running download from the same peer.

Soulbeet only ever asks AirDC++ to remove a bundle with `remove_finished` off: finished files stay, and only the partial file of an unfinished one is deleted.

## Setting up AirDC++

1. Run the AirDC++ Web Client 2.14 image, pinned: `gangefors/airdcpp-webclient:2.14.0` (the commented `airdcpp` service in `docker-compose.yml`), on a network Soulbeet reaches. Never publish its web port (5600) to the internet.
2. Mount the same downloads folder in both containers: AirDC++'s `/Downloads` and Soulbeet's `DOWNLOAD_PATH`. Run AirDC++ with Soulbeet's user and group (`PUID`, `PGID`, the same `UMASK`), so Soulbeet and beets can move what AirDC++ saves.
3. Mount nothing under AirDC++'s `/Share`: the image shares every folder mounted there with every hub. Remove its default `/Share/` share root.
4. Change the `admin` password. Create a web user for Soulbeet with exactly these permissions: `search`, `download`, `transfers`, `queue_view`, `queue_edit`, `hubs_view`, `settings_view`, `share_view`, `filelists_view`, `filelists_edit`, `filesystem_view`, `filesystem_edit`. Not an admin, no `share_edit`, no `settings_edit`: Soulbeet never changes AirDC++'s settings, hubs or share. Soulbeet refuses the image's default `admin`/`password` login.
5. In AirDC++: a nickname that does not name you, an empty description and email; partial sharing off; following redirects off; language English (Soulbeet reads AirDC++'s English messages); passive mode unless you publish its ports 21248 TCP and UDP and 21249 (active mode reaches more peers).
6. Add favorite hubs that admit a client sharing nothing, each with its encoding (CP1251 for Cyrillic hubs, CP1250 for Central European ones, CP1252 otherwise) and auto connect on, so AirDC++ connects them again after a restart. Read each hub's rules and set AirDC++'s minimum search interval to the strictest one.
7. In Soulbeet, Settings > Config > Connections, under Direct Connect: the AirDC++ address (for example `http://airdcpp:5600`), the user name and the password. The password is stored encrypted under `SECRET_KEY` and never shown again. **Check AirDC++** shows what Soulbeet sees: the AirDC++ version, the hubs connected, passive or active mode, the files shared, partial sharing, and whether AirDC++ saves into Soulbeet's downloads folder (Soulbeet has AirDC++ make a folder there and looks for it). Then turn Direct Connect on.

Soulbeet sends the user name and password with every request (HTTP Basic auth), several times a second while files download. Over plain `http://` they cross the network unencrypted, so use an `http://` address only on the private container network of the compose file or on the same machine. Reach an AirDC++ on another host over `https://`: through a TLS proxy in front of it, or AirDC++'s own TLS web port (5601) once it holds a certificate Soulbeet trusts (the one AirDC++ makes for itself is self-signed, and Soulbeet refuses it).

The status line under the search bar shows DIRECT CONNECT ONLINE once AirDC++ answers with Soulbeet's user, has every permission above, runs in English, is connected to a hub, and saves into Soulbeet's downloads folder. The folder check runs at most every 10 minutes, whatever it found, at every settings save and at every **Check AirDC++**: after fixing the folders, press Check. A folder check that fails can leave one `.soulful-check-…` folder in AirDC++'s download folder, where Soulbeet cannot delete it; delete it by hand. Saving the settings makes Soulbeet sign in with them at the next request; after AirDC++ refused a password, Soulbeet sends nothing more, **Check AirDC++** included, until the settings are saved again, so a wrong password never gets its address blocked by AirDC++ (5 failed logins in 45 seconds). Once the password is right on AirDC++'s side, save the settings again, even unchanged.

## Turning it off

Turning it off stops its searches and removes its unfinished downloads from AirDC++'s queue, which deletes their partial files. Downloads still waiting on it fail with "Direct Connect is turned off in Settings > Config > Connections". Files it finished stay in their batch folders.

## What the hubs see

Hubs and peers see the IP address of the machine AirDC++ runs on, and the nickname and share size you set. Some hubs show IP addresses in their chat. With an empty share, the only upload is AirDC++'s own empty file list.

## Rights and risks

Downloading recordings without the rights holder's permission is infringement in most places, as on Soulseek. Some hubs answer with fake results or mislabelled files: a TTH proves the bytes are the peer's file, not that the file is what its name says. Successes often depend on a few generous users, and a peer with no free slot can keep a file waiting for a long time.
