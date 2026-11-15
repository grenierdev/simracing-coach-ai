# Vérifications préalables

Relevé du 2026-11-15. [V] : vérifié dans le code source ou la documentation. [S] : supposé, reste à confirmer.

## Synthèse des décisions

| Sujet | Décision |
|---|---|
| SDK iRacing et .ibt | Format implémenté dans le crate `telemetry`, sans crate externe |
| UI | `gpui-pre` `=0.3.12` (instantané de Zed), épinglé exactement |
| ACP | `@agentclientprotocol/claude-agent-acp` 0.107.0 et crate `agent-client-protocol` 5.3.0, protocole v1 |
| Modèle | Un sous-processus d'adaptateur par rôle, choix via `ANTHROPIC_MODEL` ; changement à chaud via `session/set_config_option` (`configId: "model"`) |
| Outils intégrés | `_meta.claudeCode.options.tools: []`, `allowedTools: ["mcp__coach"]`, mode `dontAsk`, et refus côté client dans `session/request_permission` (deuxième barrière) |
| Transport MCP | HTTP streamable sur `127.0.0.1:<port éphémère>` dans le processus (`rmcp` 3.10, `transport-streamable-http-server`), avec jeton Bearer aléatoire |
| Authentification | Abonnement du CLI (login claude.ai), décidé le 2026-11-15 |
| Prompt système | Prompt du coach injecté en `override` à la place de celui de Claude Code, décidé le 2026-11-15 |
| Référence | Import manuel du CSV Garage61 ; l'API ne donne pas accès aux tours hors équipe |
| STT/TTS | moshi-server : essai de compilation native sous Windows, repli sur WSL2 ; client WebSocket et MessagePack identique dans les deux cas |
| Latence 1,5 s | **À risque**, voir la dernière section |

## 1. Crates pour le SDK iRacing et les .ibt

Crates existantes, relevées sur crates.io :

| Crate | Version / date | .ibt | État |
|---|---|---|---|
| `kerb` | 0.4.0 / 2026-09 | non | actif, `windows-sys` 0.61 |
| `pitwall` | 0.1.0 / 2025-10 | oui | une seule version, pas de dépôt public, impose tokio |
| `simetry` | 0.2.3 / 2024-02 | oui | multi-simulateurs, `windows` 0.44, plus maintenue |
| `iracing-telem` | 0.2.1 / 2023-11 | non | API presque entièrement `unsafe` |
| `itelem` | 0.1.5 / 2023-04 | oui (seulement) | plus maintenue |
| `iracing` | 0.4.1 / 2020-12 | non | `winapi`, abandonnée |

Aucune ne couvre à la fois le live et les .ibt avec une maintenance active. Le format est petit (header de 112 octets, sous-header disque de 32 octets, descripteurs de variables de 144 octets) et le même parseur sert pour les deux sources. Il est donc implémenté dans `crates/telemetry/src/irsdk.rs`. Les offsets ont été recoupés avec pyirsdk 1.3.7, `simetry` et `kerb` [V]. Je n'ai pas lu le fichier officiel `irsdk_defines.h` : il est réservé aux membres du forum iRacing.

Points de format retenus :

- Live : on prend le tampon dont le `tickCount` est le plus élevé, on copie la ligne, puis on vérifie que le `tickCount` n'a pas changé pendant la copie (`live.rs`). Les copies passent par `ptr::copy_nonoverlapping`, sans jamais former de `&[u8]` sur une mémoire modifiée par iRacing.
- Le YAML de session est en Windows-1252, ou en UTF-8 s'il déclare `Encoding: UTF8`. Les deux cas sont gérés.
- `Lat`, `Lon` et `Alt` n'existent que dans les .ibt. Tout canal autre que `SessionTime`, `Lap`, `LapDistPct` et `Speed` est donc facultatif (valeur `NaN` s'il manque).
- [S] Les versions récentes du SDK utiliseraient des champs logés dans les `pad` (`tickCountBegin` par tampon) pour détecter les lectures déchirées. On ne s'en sert pas : la double lecture du `tickCount` suffit.

## 2. gpui

- [V] La crate `gpui` de crates.io est figée en 0.2.2 (2025-10-22). Le README de Zed demande maintenant `gpui_platform`, qui n'est pas publié.
- [V] `gpui-pre` est un instantané hebdomadaire de Zed publié par Longbridge, 0.3.12 du 2026-11-09. La bibliothèque garde le nom `gpui`. C'est aussi la base de `gpui-component` 0.9.1, la bibliothèque de composants qu'on voudra probablement pour les tableaux et les champs de saisie.
- [V] Sous Windows : Win32, DirectWrite, Direct3D 11. Une compilation en release demande `fxc.exe` (Windows SDK) ou la variable `GPUI_FXC_PATH`.
- [V] Sous Linux, pour la CI : `libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libx11-xcb-dev libfreetype-dev libfontconfig-dev libwayland-dev libvulkan1`. L'app a été lancée sous Xvfb avec le Vulkan logiciel (llvmpipe) et s'affiche correctement.
- Maturité : pré-1.0, changements cassants fréquents, documentation surtout dans le code de Zed. Versions épinglées avec `=`, à mettre à jour en même temps que `gpui-component`.

## 3. ACP : versions et compatibilité

- [V] `@agentclientprotocol/claude-agent-acp` **0.107.0** (2026-11-13), sur `@agentclientprotocol/sdk` 2.6.0 et `@anthropic-ai/claude-agent-sdk` 0.3.323. L'ancien paquet `@zed-industries/claude-code-acp` est déprécié depuis la 0.16.2.
- [V] L'adaptateur répond `protocolVersion: 1` ; il sait aussi parler le brouillon v2 si le client le demande.
- [V] Des exécutables autonomes Windows embarquent l'adaptateur et `claude.exe`, sans Node. C'est la voie la plus simple à distribuer.
- [V] Crate `agent-client-protocol` **5.3.0** (2026-11-14), schéma `=1.18.0`, `ProtocolVersion::LATEST = V1` : **compatible**.
- [V] L'API Rust a changé depuis les exemples courants : `ClientSideConnection` et le trait `Client` à implémenter n'existent plus. Le client se construit par handlers, par exemple `Client.v1().on_receive_notification(...).on_receive_request(...).connect_with(AcpAgent::from_str(cmd)?, ...)`. Il faut la feature `process`, plus `unstable_session_options` pour `configOptions`. Handlers et futures doivent être `Send`, sans `LocalSet`. Le moteur n'impose pas de runtime.

## 4. Sélection du modèle et restriction des outils

Modèle [V] (`dist/session-model.js`) :

- À la création de la session, l'ordre de priorité est : `ANTHROPIC_MODEL`, puis `model` dans settings.json, puis le modèle d'une session reprise, puis le premier modèle disponible.
- En cours de session, le modèle se change par `session/set_config_option` avec `configId: "model"`. Il n'y a pas de handler `session/set_model`.
- `configOptions` dans `session/new` fonctionne aussi, mais la fonction est marquée instable.
- Choix retenu : un sous-processus d'adaptateur par rôle (conversation en piste, analyse), chacun avec son `ANTHROPIC_MODEL`. Les deux sessions sont isolées et le modèle reste configurable par rôle.

Outils [V] (`dist/acp-agent.js`) :

- `_meta.claudeCode.options.tools: []` retire tous les outils intégrés.
- `allowedTools: ["mcp__coach"]` pré-autorise tout notre serveur. Les outils MCP sont exposés sous le nom `mcp__<serveur>__<outil>`.
- Le mode `dontAsk` (dans `permissions.defaultMode` ou via `configId: "mode"`) refuse tout ce qui n'est pas pré-autorisé.
- L'adaptateur n'envoie `session/request_permission` que lorsque les règles de Claude Code donnent « ask ». Notre handler approuve quand même `mcp__coach__*` et refuse le reste, en deuxième barrière.
- `promptSuggestions: false`, sinon une suggestion de relance est générée à chaque tour.

Prompt système et CLAUDE.md [V] :

- Par défaut, `settingSources` vaut `["user","project","local"]` : le CLAUDE.md du répertoire de travail est chargé, mais aussi les réglages utilisateur du poste.
- Le champ ACP `systemPrompt: {mode: "override", content: [...]}` remplace complètement le prompt de Claude Code.
- **Décision (2026-11-15)** : le fichier `CLAUDE.md` du coach reste dans le répertoire des sessions, mais le programme le lit et l'injecte en `override`, avec `settingSources: []`. On retire ainsi le prompt système de Claude Code (environ 15 à 25 k tokens avec les outils [S]) et les réglages personnels du poste. C'est le principal levier de latence. Écart assumé par rapport au cahier des charges, où Claude Code chargeait lui-même le CLAUDE.md.
- [S] À vérifier au jalon 4 : `settingSources: []` ne doit pas couper l'accès au login. Les identifiants sont stockés à part des settings, ce qui devrait suffire.

## 5. Transport MCP

- [V] L'adaptateur annonce `mcpCapabilities: {http: true, sse: true}`. Le stdio est toujours accepté. MCP par ACP n'est pas annoncé.
- [V] `rmcp` 3.10.0 fournit `StreamableHttpService` (feature `transport-streamable-http-server`), avec `allowed_hosts` limité au loopback par défaut.
- Choix : serveur HTTP dans le processus sur `127.0.0.1` à port éphémère, déclaré dans `session/new` comme `{type: "http", name: "coach", url, headers: [{name: "Authorization", value: "Bearer <jeton aléatoire>"}]}`. Le stdio demanderait un exécutable séparé, qui devrait ensuite rejoindre l'état du programme principal.
- La crate `agent-client-protocol-rmcp` 5.3.0 (sur rmcp ^3.4) existe, à évaluer au jalon 4.

## 6. Authentification de Claude Code

- [V] L'adaptateur utilise les identifiants du CLI `claude` : soit un login claude.ai déjà fait, soit `ANTHROPIC_API_KEY`, `apiKeyHelper` ou une clé Console.
- **Décision (2026-11-15) : abonnement du CLI**, pour un usage personnel. Aucune clé API.
- Mise en place : se connecter une fois sur le poste avec le CLI (`claude`, puis `/login`).
- Le programme retire `ANTHROPIC_API_KEY` de l'environnement du sous-processus de l'adaptateur. Sinon le CLI utiliserait la clé à la place de l'abonnement, et la facturerait.
- Ne pas passer `--hide-claude-auth` à l'adaptateur : cette option refuse justement les sessions payées par abonnement.
- Pour mémoire, la documentation de l'Agent SDK n'autorise pas les produits tiers distribués à proposer le login claude.ai. Si l'application devait un jour être distribuée, il faudrait revenir à une clé API.
- Les limites d'usage de l'abonnement s'appliquent : une session d'analyse en Opus qui tourne à chaque tour consommera vite le quota. À surveiller au jalon 6.

## 7. CSV Garage61

- Colonnes probables, d'après des projets qui parsent ces exports : `Speed, LapDistPct, Lat, Lon, Brake, Throttle, RPM, SteeringWheelAngle, Gear, Clutch, ABSActive, DRSActive, LatAccel, LongAccel, VertAccel, Yaw, YawRate, PositionType`. [S] : pas vérifié sur un export réel, garage61.net n'était pas joignable depuis cette session. `YawRate` est la colonne la moins sûre.
- Absents : `SessionTime`, `LapDist` et le numéro du tour. Le temps se déduit de l'index (60 Hz) ; l'alignement se fait sur `LapDistPct`, qui est bien présent. Les derniers pourcents du tour manquent parfois.
- Si `YawRate` manque mais que `Yaw` est présent : dériver `Yaw` déroulé dans le temps.
- Si `YawRate` et `LatAccel` manquent :
  1. Projeter Lat/Lon en mètres locaux.
  2. Rééchantillonner selon l'abscisse curviligne.
  3. Lisser (Savitzky-Golay ; la résolution de Lat/Lon en float32 est d'environ 0,4 m).
  4. Calculer la courbure κ, puis `YawRate ≈ v·κ` et `LatAccel ≈ v²·κ`.

  Cette estimation ignore la dérive, le dévers et la gravité : elle suffit pour comparer des trajectoires, pas pour mesurer un survirage fin.
- La détection des virages du crate `telemetry` utilise déjà `YawRate / Speed`, avec repli sur `LatAccel / Speed²`. Les deux voies sont testées.

## 8. API Garage61

- [V, via la documentation recopiée dans le wrapper `garage61api`] Base `https://garage61.net/api/v1`, `Authorization: Bearer <jeton>` (jeton personnel ou OAuth2 PKCE, scope `driving_data`). Endpoints : `GET /laps` (filtres voiture, piste, pilotes, équipes) et `GET /laps/{id}/csv`.
- [V] « This method only returns laps driven yourself or your teammates… For privacy reasons, no global lap search is available. » Il n'y a **pas d'accès API aux tours de pilotes hors équipe**.
- [S] Les data packs publiés par des équipes ont un endpoint dédié (`/teams/{team}/datapacks/...`). Qu'un simple abonné puisse les télécharger par l'API n'est pas vérifié.
- Décision : **import manuel du CSV** au jalon 2. L'import par l'API pour ses propres tours et ceux de son équipe viendra en option, avec le jeton dans `GARAGE61_TOKEN`.

## 9. moshi-server (Kyutai)

- [V] `moshi-server` 0.6.4. Authentification par l'en-tête `kyutai-api-key` (valeur listée dans `authorized_ids`). Tous les messages sont du MessagePack étiqueté par `type`.
- [V] STT, `/api/asr-streaming` :
  - envoi : `{"type":"Audio","pcm":[f32]}`, mono 24 kHz, blocs de 1920 échantillons (80 ms), et `{"type":"Marker","id":n}` ;
  - réception : `Word{text,start_time}`, `EndWord{stop_time}`, `Step{prs}` (VAD), `Marker`, `Ready`, `Error`.
  - Modèle `kyutai/stt-1b-en_fr` : anglais et français, délai de 0,5 s.
- [V] TTS, `/api/tts_streaming?voice=...&format=PcmMessagePack` :
  - envoi : `{"type":"Text","text":...}` mot par mot, puis `{"type":"Eos"}` ;
  - réception : `Audio{pcm}` à 24 kHz, `Text{text,start_s,stop_s}`.
  - Modèle `kyutai/tts-1.6b-en_fr`. La config fournie utilise le module Python embarqué ; les réglages CFG sont dans la config, pas dans l'URL.
- [S] Voix françaises : `cml-tts/fr/` (CC-BY 4.0) dans `kyutai/tts-voices`. Les voix `expresso/` sont non commerciales. Je n'ai pas pu lister le dépôt (huggingface.co bloqué).
- [V] Windows : le README de moshi dit « While we hope that the present codebase will work on Windows, we do not provide official support for it ». Aucun rapport de compilation native trouvé.
- [V] Le code Rust de `moshi-server` 0.6.4 n'a aucune partie propre à Unix (pas de `cfg(unix)`, `libc` ni `nix`). Les obstacles sous Windows viennent de la chaîne de compilation :
  - CUDA : `nvcc` et MSVC, pour candle avec la feature `cuda` ;
  - libopus : la crate `opus` la compile en C ;
  - Python : pyo3 lie libpython, même pour le STT seul.
- [S] Le module TTS est en Python embarqué. Ses dépendances (`pyproject.toml`) et le script de démarrage (`start_tts.sh`, en bash) sont pensés pour Linux.
- Plan :
  1. Tenter la compilation native sur le PC de simracing, au jalon 3.
  2. En cas d'échec, WSL2 avec CUDA. WSL2 accède au GPU NVIDIA avec les pilotes récents, et le port du serveur est joignable depuis Windows par `localhost`.
  3. Une machine Linux séparée reste possible.

  Le client est le même dans les trois cas : WebSocket et MessagePack.
- [S] Partage du GPU avec iRacing : le STT (environ 1 milliard de paramètres) et le TTS (environ 1,6 milliard) tourneront sur la même carte que le jeu. Il faudra mesurer la VRAM et l'effet sur les images par seconde. Réduire `batch_size` (64 dans la config d'exemple) à 1 ou 2 diminue la mémoire réservée.

## 10. Latence de la conversation

Rien n'est encore mesuré : la mesure se fera au jalon 4, avec les horodatages par étape affichés dans l'UI. Ce qui est établi [V] :

- `session/new` attend l'initialisation complète du CLI. Le démarrage à froid se paie donc à la création de session, d'où l'ouverture des sessions au lancement de l'app.
- L'adaptateur journalise ses phases (`SessionTiming`), ce qui aidera à ventiler les temps.

Estimations à confirmer [S] :

| Étape | Estimation |
|---|---|
| STT : du relâchement du bouton au texte final | 0,5 à 0,7 s (délai du modèle 0,5 s, plus le vidage par Marker et silence) |
| Claude : du prompt au premier `agent_message_chunk`, Haiku, session chaude | 0,7 à 1,5 s avec prompt en `override` et `tools: []` ; 1,5 à 3 s avec le prompt de Claude Code |
| TTS : du premier texte au premier son | 0,2 à 0,35 s |
| **Total** | **1,4 à 2,5 s** |

La cible de 1,5 s n'est tenable qu'avec le prompt en `override`, le cache de prompt chaud, Haiku sans réflexion et un TTS qui démarre sur les premiers mots. Leviers si ça ne passe pas :

- envoyer un accusé court local (pré-enregistré) dès la fin de parole ;
- commencer la transcription pendant la parole et non au relâchement ;
- en dernier recours, appeler l'API Messages directement pour la session de conversation, en gardant ACP pour l'analyse.
