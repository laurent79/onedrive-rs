# onedrive-rs — client OneDrive pour Linux en Rust, avec GUI intégrée

Réécriture en Rust du client D [`abraunegg/onedrive`](https://github.com/abraunegg/onedrive) (~43k lignes,
dont `src/sync.d` 19k). Objectifs : mémoire faible et prévisible, concurrence maîtrisée, GUI native.
Licence : GPL-3.0-only (œuvre dérivée du client D).

Notes propres à une machine (comptes, dossiers, état d'un déploiement) : `CLAUDE.local.md`, non versionné.

**Référence fonctionnelle :** `docs/client-architecture.md` du client D (spécification de fait) et son code.
Toujours vérifier le comportement du client D avant de réinventer un cas particulier.

## Architecture

- Un seul binaire : `onedrive-rs daemon` (service `systemd --user`, sans interface) et `onedrive-rs gui`
  (fenêtre + icône de notification) qui pilote le daemon par **D-Bus** (`zbus` 5, interface
  `io.github.onedrive_rs.OneDrive1` sur `/io/github/onedrive_rs/OneDrive1`). La GUI ne fait jamais d'E/S de synchronisation.
- Workspace Cargo :

  | Crate | Rôle |
  |---|---|
  | `crates/quickxor` (`odrs-quickxor`) | QuickXorHash (aussi état repris pour les téléchargements partiels) |
  | `crates/graph` (`odrs-graph`) | OAuth2 (loopback + manuel, PKCE), client Graph (retries, throttling partagé), delta, download/upload, sites |
  | `crates/store` (`odrs-store`) | SQLite (`rusqlite` bundled), **même schéma que le client D** (v18) |
  | `crates/engine` (`odrs-engine`) | Réconciliation : décisions pures (`convert`, `decide`, `moves`, `filter`), E/S en bordure (`download`, `upload`) |
  | `crates/ipc` (`odrs-ipc`) | Interface D-Bus : `Server` (daemon), `Client`/`OneDriveProxy` (GUI, CLI), trait `DriveManager` |
  | `crates/watch` (`odrs-watch`) | inotify (`notify` 8.2) : signale *qu'il* y a un changement local |
  | `crates/gui` (`odrs-gui`) | Slint 1.18, rendu logiciel, `SystemTrayIcon` intégré (ksni) ; traductions `.po` embarquées |
  | `app` (`onedrive-rs`) | CLI `clap`, config, daemon, `import-d` |
- Principes mémoire : `/delta` traité **page par page** (serde typé) ; SQLite = seule source de vérité ;
  transferts concurrents bornés (`JoinSet`, `threads`) ; corps d'upload et de download **streamés** (64 Kio) ;
  parcours local dossier par dossier. Ordre de grandeur mesuré : quelques dizaines de Mo de RSS pour des
  centaines de milliers d'items sur plusieurs drives, contre plusieurs Go pour le client D.
- HTTP : `reqwest` 0.13 (`rustls,json,stream,http2,form,query`, pas d'OpenSSL).
- GUI : Slint plutôt que GTK4 (cible KDE Plasma ; libadwaita y détonne). `renderer-software` seulement :
  ~28 Mo RSS contre ~83 Mo avec femtovg/Mesa.
  Fenêtres créées à leur première ouverture (`lazy` dans `crates/gui/src/lib.rs`) : Slint/winit crée le toplevel Wayland
  dès la création du composant, et Plasma liste ces fenêtres jamais montrées comme entrées vides de la barre des tâches
  (`hide()` ne les détruit pas ; une fenêtre montrée puis fermée, si). Seule l'icône de notification existe au départ.

## Comportement de synchronisation (aligné sur le client D)

- Ordre d'une passe (D « remote-first ») : delta en ligne appliqué localement, puis parcours local et
  envoi. Une passe = toutes les cibles (`main` = OneDrive du compte, puis chaque `[drive]`) ; un échec
  n'arrête pas les autres.
- Téléchargement : `<fichier>.partial`, hash QuickXor vérifié, mtime fixé, renommage atomique ; reprise par
  `Range` (préfixe re-hashé) ; `.partial` supprimé seulement si le hash est faux. deltaLink enregistré
  seulement si la passe n'a eu aucune erreur ; 410 → énumération complète.
- **« En sync » = mtime identique à la seconde, quelle que soit la taille** (`isItemSynced` de D) : SharePoint
  enrichit les fichiers Office après upload (DB = taille/hash en ligne, fichier local plus petit).
- Conflit (modifié des deux côtés) : jamais d'écrasement ; version locale gardée en
  `<nom>-<hôte>-safeBackup-NNNN<ext>` (format D), version en ligne téléchargée tout de suite (412 à l'upload).
- Suppression en ligne → suppression locale seulement si le fichier local est inchangé. Suppression locale
  → suppression en ligne après le parcours (`If-Match`), refusée au-delà de `classify_as_big_delete` (1000).
  En bidirectionnel, un item synchronisé absent localement dont le contenu en ligne n'a pas changé est une
  suppression locale : la passe download ne le restaure pas (`decide::is_local_deletion`).
- Déplacements/renommages locaux détectés avant le parcours (`moves.rs`) : dossier = même ensemble de noms
  d'enfants, fichier = taille + mtime puis hash → un seul `PATCH`.
- Upload : simple ≤ 4 Mio, sinon session (fragments 10 Mio) ; différé si le fichier a moins de 10 s ;
  `If-Match` cTag|eTag. Après un simple upload, mtime local aligné sur celui renvoyé (comme D).
  Progression : `Stats::upload_queued[_bytes]` (envois lancés) et `upload_sent_bytes` (octets acceptés, fragment par
  fragment via `UploadHooks::sent`) → `Progress::upload_*` → barre « Envoi » de la GUI au-delà de 8 Mio.
- Reprise des sessions d'upload (`SessionStore`, `crates/engine/src/sessions.rs`) : un JSON 0600 par envoi dans
  `<config>/upload_sessions/` (URL pré-authentifiée, taille, mtime, expiration), réutilisé seulement si le fichier est
  inchangé ; reprise à `nextExpectedRanges`. Avant de reprendre un remplacement, cTag/eTag en ligne revérifié (sinon la
  reprise écraserait une modification en ligne). 403/404/410/416 = session perdue (jeton `tempauth` expiré, cf. D) →
  nouvelle session ; erreur transitoire → session gardée pour la passe suivante ; sessions expirées purgées.
- Filtres (deux sens) : `skip_file` (défaut `~*|.~*|*.tmp|*.swp|*.partial`, les entrées utilisateur
  s'ajoutent), `skip_dir` (nom partout / chemin exact), `skip_dotfiles`, noms refusés par OneDrive. Une section
  `[drive]` peut redéfinir `skip_*`/`sync_root_files` (remplacent les valeurs globales pour ce drive).
- Sync sélective (`filter::SyncList`, `sync_list` de D) : `<config>/sync_list` (main), `sync_list.<nom>` (drive),
  relu à chaque passe. Tout exclu sauf inclusions ; exclusions (`!`/`-`) prioritaires ; `/` initial = ancré, sinon
  à toute profondeur (tous les dossiers deviennent alors des chemins possibles) ; `/` ou `/*` final = dossier ;
  `*`, `?`, `**` ; insensible à la casse ; les dossiers menant à une inclusion sont synchronisés (sans le reste).
  Sémantique documentée de D, pas ses heuristiques (sous-chaînes, regex). Règles refusées comme D (`/`, `/*`, `./`).
- Changement de filtres (`selective.rs`) : empreinte des règles par drive dans `<config>/filters/` ; si elle change,
  lignes DB exclues oubliées (copies locales gardées : exclues, donc jamais envoyées), deltaLink effacé → énumération
  complète qui rattache sans télécharger. Première fois : empreinte seulement enregistrée. D exige `--resync`.
  Élément suivi déplacé en ligne vers une zone exclue = suppression locale (si inchangé) ; parent hors base reconnu
  seulement s'il a été exclu pendant la passe (`excluded_dirs`, comme `skippedItems` de D : `/delta` ne garantit
  pas l'ordre parent/enfant). Côté envoi, une ligne DB exclue n'est jamais une suppression en ligne.
- Choix des dossiers dans la GUI (`FoldersWindow`, `crates/gui/src/folders.rs`) : D-Bus `ListFolders` (sous-dossiers en
  ligne d'un chemin, `Graph::child_folders`, état `SyncList::folder_state` : inclus / `mixed` = une règle ancrée vise
  plus bas), `CustomRules`, `SetSelection(choix, delete_local)`. Règles générées par `filter::selection_rules` :
  `/dossier/` et `!/dossier/` seulement ; règles non représentables (jokers, sans `/`) et règles sous un dossier ni
  ouvert ni modifié recopiées telles quelles ; tout coché = fichier supprimé. Une exclusion l'emporte toujours (D) :
  dossier décoché contenant un sous-dossier coché → gardé avec exclusions des autres. Écrit aussi
  `sync_root_files = "true"` (fichiers à la racine toujours synchronisés, comme le client Windows).
  `delete_local` → `SyncOptions::delete_excluded` jusqu'à une passe réussie du drive : copies locales supprimées si
  mtime = DB, dossiers seulement s'ils deviennent vides.
- Permissions : dossiers 0700, fichiers 0600 (défauts D).
- Changements en ligne quasi instantanés (`odrs_graph::realtime`, comme `socketio.d` de D) : par drive,
  `GET /drives/{id}/root/subscriptions/socketIo` → `notificationUrl`, WebSocket Socket.IO v4 (`40`, `40/notifications`,
  pong `3`), événement `notification` = « lancer /delta » → passe 2 s après la dernière. URL renouvelée 120 s avant
  expiration, reconnexion 1→60 s, silence > pingInterval+pingTimeout = connexion morte. TLS = même pile que reqwest
  (rustls + aws-lc-rs + vérificateur plateforme), `tokio-tungstenite` sans TLS. `disable_websocket_support` (option D).
  Le RSS inclut ~16 Mo pour les 4 connexions (58 Mo mesuré) ; `MemoryPeak` systemd compte aussi le cache disque.
- Daemon : intervalle 300 s ; inotify (bidirectionnel) → passe 5 s après le dernier changement ; hors ligne →
  nouvelle passe après 60 s ; login expiré → `LastError` préfixé `AUTH_ERROR_PREFIX` (bouton GUI).
- Resync d'un drive (`--resync` de D ; CLI `resync --drive`, D-Bus `Resync`, icône GUI avec confirmation) :
  `Store::delete_drive` efface ses lignes et son deltaLink entre deux passes, puis passe immédiate ; le moteur
  rattache les fichiers locaux identiques (hash) sans les retélécharger, les différents → safeBackup puis
  téléchargement. Sans référence, les suppressions d'un seul côté réapparaissent (comme D). La CLI ne passe
  par D-Bus que si le verrou de *son* dossier de config est pris (le bus peut montrer le daemon d'une autre config).
- Notifications bureau (`org.freedesktop.Notifications`) envoyées par la GUI (`crates/gui/src/notify.rs`) : elle
  compare deux instantanés du daemon (1er = référence, rien de passé n'est notifié). Login expiré, erreur d'un drive
  (une fois par message, chiffres ignorés ; erreurs réseau préfixées `OFFLINE_ERROR_PREFIX` ignorées), conflits
  (safeBackup, regroupés au-delà de 3), fin de synchro complète (`Stats::full_enumeration` : 1re synchro, resync,
  410), fin de vérification. Conflits et synchro complète passent par la propriété D-Bus `Events` (ids croissants
  même après redémarrage du daemon). Textes traduits via `Fmt.note-*` dans le Slint.
- Vérification d'intégrité (`odrs_engine::verify`, CLI `verify` et D-Bus `Verify`/`CancelVerify`/`VerifyReport`,
  propriété `Verifications`) : lecture seule, un drive à la fois à côté des passes (2 threads, connexion SQLite
  propre) ; écarts revérifiés à la fin (ligne DB changée = la synchro est passée par là → ignoré). Dernier
  résultat par drive dans `verify.json` (dossier de config), 1000 écarts max.

## Faits extraits du client D

**Auth** (`src/onedrive.d` l.320-480, `src/localAuth.d`) :
- `application_id` par défaut `d50ca740-c83f-4d1b-b616-12c519384f0c` (app publique d'abraunegg, configurable).
- Endpoints `https://login.microsoftonline.com/common/oauth2/v2.0/{authorize,token}` ; scopes
  `Files.ReadWrite Files.ReadWrite.All Sites.ReadWrite.All offline_access` (lecture seule : `*.Read*`).
- Loopback `http://127.0.0.1:<port>/`, port libre dans **53100–53149**, `state` validé ; repli redirect
  `https://login.microsoftonline.com/common/oauth2/nativeclient`. Client public ; nous ajoutons PKCE S256.

**Graph** :
- Delta : `/drives/{driveId}/items/{rootId}/delta?select=id,name,eTag,cTag,deleted,file,folder,root,fileSystemInfo,remoteItem,parentReference,size,createdBy,lastModifiedBy,package`,
  suivre `@odata.nextLink`, stocker `@odata.deltaLink` dans la colonne `deltaLink` de la ligne racine.
- `Prefer: Include-Feature=AddToOneDrive` : compte personnel seulement (Business : si `sync_business_shared_items`).
- User-Agent ISV : `ISV|onedrive-rs|onedrive-rs/<version>`.
- Ignorer paquets OneNote (`package.type == "oneNote"`), malware ; enfants dont le parent n'est pas en DB.

**Schéma DB** (`src/itemdb.d` l.495, `PRAGMA user_version = 18`) :
```sql
CREATE TABLE item (
  driveId TEXT NOT NULL, id TEXT NOT NULL, name TEXT NOT NULL, remoteName TEXT,
  type TEXT NOT NULL, eTag TEXT, cTag TEXT, mtime TEXT NOT NULL, parentId TEXT,
  quickXorHash TEXT, sha256Hash TEXT, remoteDriveId TEXT, remoteParentId TEXT, remoteId TEXT,
  remoteType TEXT, deltaLink TEXT, syncStatus TEXT, size TEXT, relocDriveId TEXT, relocParentId TEXT,
  PRIMARY KEY (driveId, id),
  FOREIGN KEY (driveId, parentId) REFERENCES item (driveId, id) ON DELETE CASCADE ON UPDATE RESTRICT);
CREATE INDEX name_idx ON item (name);
CREATE INDEX remote_idx ON item (remoteDriveId, remoteId);
CREATE INDEX item_children_idx ON item (driveId, parentId);
CREATE INDEX selectByPath_idx ON item (name, driveId, parentId);
```
- `type`/`remoteType` ∈ `file|dir|remote|root|unknown` ; `mtime` ISO UTC à la seconde, uniquement depuis
  `fileSystemInfo.lastModifiedDateTime` ; `size` en TEXT ; `syncStatus = "Y"` ; racine sans `parentId`.
- Upsert `ON CONFLICT DO UPDATE`, **jamais `INSERT OR REPLACE`** (supprime la ligne → cascade sur les enfants).
- Plusieurs drives dans une même DB (lignes distinguées par `driveId`) : `import-d` fusionne la DB d'une
  instance D dédiée à une bibliothèque (`Store::merge_drive`). `import-d` reprend aussi les `skip_*`,
  `sync_root_files` et le `sync_list` de D (la DB importée en tient compte).

## Développement

- `cargo test` ; scénarios d'upload sans réseau : `crates/engine/tests/upload_dry_run.rs` (mode dry-run).
- `cargo run -p odrs-ipc --example mock_daemon` : faux daemon (drives, sites) pour travailler la GUI.
- Rendu d'une fenêtre sans capturer le bureau : `kwin_wayland --virtual --socket odrs-snap --exit-with-session
  "target/debug/examples/snapshot <main|resync|report|folders> /tmp/w.rgba"` (`LANG=fr_FR.UTF-8` pour le français), puis
  `magick -size <LxH affiché> -depth 8 rgba:/tmp/w.rgba w.png`. Le texte replié est mesuré sur une ligne par
  Slint : donner un `min-height` aux fenêtres de texte.
- `cargo run -p odrs-graph --example download -- <refresh_token> <drive> <item> <dest>` : test de reprise.
- `onedrive-rs verify [--drive NAME] [--limit N]` : recalcule les QuickXorHash de chaque drive contre son dossier
  (lecture seule, daemon actif possible ; code de sortie 1 si fichier modifié/absent/illisible). Fichier au
  mtime de la DB mais au contenu différent = en sync pour D (Office enrichi par SharePoint), compté à part.
  `--db <copie> --dir <dossier>` : vérifie une autre DB (ex. celle du client D).
- Pour inspecter une DB du client D en cours d'exécution : la copier (`items.sqlite3*`), elle est verrouillée.
- Tests d'écriture sur un vrai compte : uniquement dans un dossier bac à sable, supprimé ensuite.

## Problèmes connus / à faire

- Icône de fenêtre absente dans la barre des tâches KDE (Wayland) malgré `app_id` = `io.github.onedrive_rs`
  et `.desktop` installé ; piste : `xdg_toplevel_icon_v1` (non géré par winit/Slint).
- À faire : éléments partagés (`remote`), progression des envois, application Entra dédiée, paquet RPM.

## Conventions

- Code, commentaires et documentation du dépôt en anglais ; échanges avec le mainteneur en français.
- Erreurs : `thiserror` dans les crates, `anyhow` dans `app`. Logs : `tracing`.
- Branche `main`.
