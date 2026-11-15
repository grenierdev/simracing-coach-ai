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
| STT/TTS | **sherpa-onnx 1.13.8** embarqué dans le processus (décidé le 2026-11-15) : Parakeet v3 pour le STT, Piper `fr_FR-siwis` pour le TTS ; Kyutai abandonné. Mesuré : STT 0,2 à 0,4 s, premier son TTS 0,1 à 0,2 s sur CPU |
| Latence 1,5 s | **Non tenue à froid** : premier mot de Claude mesuré à 1,78 s (Haiku, cloud) ; à remesurer sur le PC, voir la dernière section |

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
- La page développeur de garage61.net est une application JavaScript : sa documentation n'est pas lisible sans navigateur, et n'a donc pas pu être revérifiée depuis la session.
- Décision : **import manuel du CSV** au jalon 2. L'import par l'API pour ses propres tours et ceux de son équipe viendra en option, avec le jeton dans `GARAGE61_TOKEN`.

## 9. STT et TTS : sherpa-onnx (décision du 2026-11-15)

Kyutai (moshi-server) est écarté : pas de prise en charge officielle de Windows, un module TTS en Python embarqué, CUDA obligatoire et un serveur séparé. Les notes sur son protocole sont dans l'historique git de ce fichier.

Retenu : **sherpa-onnx**, embarqué dans le processus, sans serveur, sans Python et sans WSL.

### Version : 1.13.8, pas 2.x

- [V] Depuis la 2.0, sherpa-onnx n'embarque plus le phonémiseur espeak-ng pour les voix Piper/VITS : `--vits-data-dir` est ignoré, et la synthèse échoue faute de `phoneme_codepoints` fournis par un phonémiseur externe. Testé avec la 2.0.1.
- **Décision : épingler la crate `sherpa-onnx` en `=1.13.8`**, dernière version 1.x, qui phonémise encore elle-même. Son API Rust est la même que celle de la 2.0.1 pour notre usage.
- Pour passer plus tard à la 2.x, il faudra un phonémiseur externe. Candidats : la crate `espeak-ng` (portage Rust pur, 0.2.0) ou des liaisons à la bibliothèque C.
- Licence : espeak-ng est sous GPL-3, et il est lié dans la bibliothèque sherpa-onnx 1.x. Aucun problème pour un usage personnel ; à revoir si l'application devait être distribuée.

### Distribution de la bibliothèque

- [V] Le build script de `sherpa-onnx-sys` télécharge une bibliothèque précompilée depuis les releases GitHub (statique par défaut). Sur un poste avec un accès Internet normal, `cargo build` suffit.
- [V] Les mêmes bibliothèques sont publiées sur npm (`sherpa-onnx-linux-x64`, `sherpa-onnx-win-x64`) et sur PyPI (`sherpa-onnx-core`). Pour s'en servir : feature `shared`, plus `SHERPA_ONNX_LIB_DIR` qui pointe vers le dossier de la bibliothèque. C'est ce qui a servi aux mesures ci-dessous, les releases GitHub étant inaccessibles depuis la session cloud.

### Modèles français (vérifiés sur Hugging Face, dépôts `csukuangfj/...`)

| Usage | Modèle | Taille | Licence |
|---|---|---|---|
| STT | `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` (25 langues européennes) | ~670 Mo | CC-BY 4.0 (NVIDIA) [S] |
| STT, alternative | `sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8` | plus petit | [S] |
| TTS | `vits-piper-fr_FR-siwis-medium` (voix féminine) | 63 Mo | CC-BY 4.0 [V] |
| TTS | `vits-piper-fr_FR-tom-medium` (voix masculine) | 61 Mo | **AGPLv3** [V] : à éviter |
| TTS | autres voix Piper `fr_FR` : gilles, mls, upmc, tjiho, miro | | à vérifier une par une |
| TTS | Kokoro multilingue, voix `ff_siwis` (locuteur 30) | ~300 Mo [S] | Apache 2.0 [S] |

### Mesures (session cloud, 4 vCPU partagés, sans GPU, 4 threads)

| Étape | Mesure |
|---|---|
| Chargement TTS Piper / STT Parakeet | 1,0 s / 2,1 s, une fois au démarrage |
| TTS : premier bloc audio, phrase de 2 à 3,4 s | 80 à 180 ms (environ 20 fois plus rapide que le temps réel) |
| STT : phrase de 2 à 3,4 s | 210 à 360 ms |
| STT : enregistrement réel de 5 s (`test_wavs/fr.wav`) | 415 ms, transcription exacte |

Phrases de coaching synthétisées puis retranscrites :

- « Freine cinq mètres plus tard au T5. » → « Freine 5 mètres plus tard au T5. »
- « Tu perds deux dixièmes au virage quatre, reprends les gaz plus tôt. » → « Tu perds deux dixièmes au virage 4. Reprends les gaz plus tôt. »
- « Je n'ai plus de rotation au T5, qu'est-ce que je change ? » → identique.

Parakeet écrit les nombres en chiffres, ce qui facilite le repérage des virages (« T5 », « virage 4 »). Un PC de bureau récent devrait faire mieux que ces vCPU partagés.

### Usage prévu

- STT en push-to-talk : on accumule l'audio pendant l'appui et on le transcrit d'un coup au relâchement.
- TTS phrase par phrase, au fil des `agent_message_chunk`.
- Le crate `voice` exposera des traits `Stt` et `Tts` pour pouvoir brancher d'autres moteurs : voix Windows SAPI/WinRT en repli, Kokoro pour comparer.
- Capture et lecture audio : `cpal` (WASAPI sous Windows).
- Les modèles sont téléchargés au premier lancement depuis Hugging Face dans le répertoire de données de l'utilisateur, et non versionnés.
- Les téléchargements utilisent le magasin de certificats du système (`ureq` avec `platform-verifier`) plutôt qu'une liste intégrée : ils passent derrière un proxy d'entreprise qui inspecte le TLS. Vérifié depuis la session cloud, qui passe par un tel proxy.
- Windows : la bibliothèque statique est compilée avec le runtime C statique (`/MT`) ; `.cargo/config.toml` active `+crt-static` pour la cible MSVC. Vérifié : la CI Windows compile, lie et passe les tests avec la bibliothèque statique (run du 2026-11-15, commit 08e9af8).

## 10. Entrées : push-to-talk en arrière-plan

- [V] `gilrs` 0.11 utilise par défaut Windows Gaming Input, qui « requires an in focus window to be associated with the process to receive events » (README de gilrs). Inutilisable pendant qu'iRacing a le focus. Son backend XInput fonctionne en arrière-plan mais ne voit que les manettes Xbox, pas les volants.
- Retenu sous Windows : **DirectInput 8**, coopération `DISCL_BACKGROUND | DISCL_NONEXCLUSIVE`, sur une fenêtre cachée de premier niveau. Classe `DI8DEVCLASS_GAMECTRL` (volants, pédaliers, boîtiers, joysticks, manettes). La crate `windows` n'expose pas le format prédéfini `c_dfDIJoystick2` : format maison de 128 boutons et 4 chapeaux, tous facultatifs.
- Clavier et souris : `device_query` (`GetAsyncKeyState` sous Windows, X11 sous Linux), lecture globale indépendante du focus.
- Linux (développement) : `gilrs` (evdev, fonctionne en arrière-plan).
- Vérifié : la capture et le suivi du push-to-talk fonctionnent avec le vrai clavier (X11, sous Xvfb + xdotool) ; le code DirectInput se compile et passe clippy pour la cible Windows. **Non vérifié** sur un vrai volant ou boîtier : à tester sur le PC.

## 11. Planification (jalon 6)

- Modèle de l'analyse : `ANTHROPIC_MODEL=sonnet` pour le sous-processus d'analyse, réflexion non coupée (pas de contrainte de latence : l'analyse tourne en arrière-plan).
- Deux sous-processus d'adaptateur (conversation, analyse), chacun avec son point d'accès MCP. Le modèle de chaque rôle reste configurable (`--model`, `--analysis-model`).
- Non mesuré avec Claude : durée réelle d'une analyse et consommation de quota par tour. Vérifié avec un faux agent : déclenchement, appel réel d'`update_plan` par le serveur MCP, couche du coach appliquée, plan du pilote intact.

## 12. Latence de la conversation

### Mesure réelle (2026-11-15)

Mesurée avec l'exemple `probe` du crate `coach`, depuis la session cloud, adaptateur 0.107.0, Haiku, réflexion coupée, prompt du coach en `override`, sans outils :

| Étape | Mesure |
|---|---|
| Démarrage de l'adaptateur jusqu'à la session prête (`initialize` + `session/new`) | 2,6 s, payé une fois au lancement |
| Envoi du message → premier `agent_message_chunk` | **1,78 s** |
| Envoi du message → fin du tour | 2,13 s |

Une seule mesure, à froid (premier message de la session, cache de prompt vide), depuis un conteneur cloud : à refaire sur le PC du pilote, où le réseau et le cache diffèrent. La réponse respectait le prompt du coach (deux phrases, en français, sans mise en forme).

Note : cette mesure n'était pas prévue. L'adaptateur avait été lancé avec un environnement vide pour vérifier seulement `initialize` et `session/new` sans identifiants ; l'infrastructure réseau de la session cloud a authentifié l'appel d'elle-même. Aucun autre appel réel n'a été fait.

### Budget

| Étape | Estimation ou mesure |
|---|---|
| STT : du relâchement du bouton au texte final | **mesuré** : 0,2 à 0,4 s (Parakeet v3 int8, CPU) |
| Claude : premier mot | **mesuré** : 1,8 s à froid depuis le cloud ; 1 à 1,5 s attendu avec cache chaud |
| TTS : de la première phrase au premier son | **mesuré** : 0,08 à 0,18 s (Piper, CPU), plus la latence de la sortie audio |
| **Total** | **2,1 à 2,4 s à froid** ; 1,4 à 2 s attendu avec cache chaud |

**La cible de 1,5 s n'est pas tenue** sur la seule mesure disponible ; Claude représente l'essentiel du délai. Leviers, du plus simple au plus lourd :

- l'accusé court du coach (« Je regarde le T5. ») est dit dès le premier morceau : il masque la durée des appels d'outils, pas celle du premier mot ;
- garder la session chaude et le prompt système stable pour profiter du cache de prompt (premier message plus lent que les suivants) ;
- jouer un accusé local pré-enregistré dès la fin de la parole (« Reçu. »), sans attendre Claude ;
- en dernier recours, appeler l'API Messages directement pour la conversation, en gardant ACP pour l'analyse.

L'interface affiche la latence du premier mot à chaque tour ; la mesure sur le PC du pilote décidera.
