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
- Filtres (deux sens) : `skip_file` (défaut `~*|.~*|*.tmp|*.swp|*.partial`, les entrées utilisateur
  s'ajoutent), `skip_dir` (nom partout / chemin exact), `skip_dotfiles`, noms refusés par OneDrive.
- Permissions : dossiers 0700, fichiers 0600 (défauts D).
- Daemon : intervalle 300 s ; inotify (bidirectionnel) → passe 5 s après le dernier changement ; hors ligne →
  nouvelle passe après 60 s ; login expiré → `LastError` préfixé `AUTH_ERROR_PREFIX` (bouton GUI).
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
  instance D dédiée à une bibliothèque (`Store::merge_drive`).

## Développement

- `cargo test` ; scénarios d'upload sans réseau : `crates/engine/tests/upload_dry_run.rs` (mode dry-run).
- `cargo run -p odrs-ipc --example mock_daemon` : faux daemon (drives, sites) pour travailler la GUI.
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
- À faire : resync d'un drive, notifications bureau, changements en ligne quasi
  instantanés (webhooks/WebSocket), sync sélective (`sync_list`), éléments partagés (`remote`), reprise des
  uploads en session après redémarrage, progression des envois, application Entra dédiée, paquet RPM.

## Conventions

- Code, commentaires et documentation du dépôt en anglais ; échanges avec le mainteneur en français.
- Erreurs : `thiserror` dans les crates, `anyhow` dans `app`. Logs : `tracing`.
- Branche `main`.
