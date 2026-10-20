# Spec — terminal web à sortie riche (Rust)

Oct 20, 2026 · @Thomas

## Contexte et objectif

Construire un émulateur de terminal en Rust dont la zone d'affichage est une webview : la sortie classique (texte, couleurs ANSI, curseur) est rendue comme dans n'importe quel terminal, mais un programme peut aussi y insérer du HTML, du Markdown rendu, des images et du SVG, inline, à la position du curseur. Nom de travail : `rt` (binaire du terminal `rt`, outil compagnon `rat`, jeu de mots sur `cat`, à renommer librement).

Le besoin de départ : lire des rapports Markdown avec des tableaux larges (générés par Claude Code) sans passer par un terminal qui tronque à 120 colonnes ni par un navigateur à part. La référence existante est DomTerm (BSD, 2015), qui prouve que le modèle marche mais dont le code, l'interface et le site sont datés. On ne reprend rien de DomTerm, on refait une version minimale et propre.

Ce que ça doit faire :

- Lancer un shell dans un pty et se comporter comme un terminal correct pour l'usage quotidien : prompt, ls, git, cargo, couleurs 256 et 24 bits, scrollback, redimensionnement, copier-coller.
- Accepter une séquence d'échappement propriétaire qui injecte un bloc riche (HTML, Markdown, image, SVG) dans le flux de sortie.
- Fournir l'outil CLI `rat` qui émet ces séquences depuis un fichier ou stdin, avec repli en texte brut hors de `rt`.
- Gérer l'écran alternatif (vim, htop, less) correctement, même si le rendu y est moins soigné.

Ce que ça ne doit pas faire : remplacer un terminal GPU comme Alacritty ou Ghostty sur la performance, ni implémenter les protocoles graphiques kitty ou Sixel (voir Hors périmètre).

## Architecture

Un seul binaire Rust, un seul processus, deux threads principaux : la boucle pty et la boucle d'événements de la fenêtre.

&#91;embedded content: architecture · shell, coeur Rust, webview\]

Les programmes lancés dans le shell écrivent des octets dans le pty comme dans n'importe quel terminal. Le cœur Rust lit ces octets, les passe au parseur ANSI, intercepte la séquence OSC propriétaire avant qu'elle n'atteigne la grille, et pousse vers la webview des événements JSON décrivant ce qui change. La webview ne tient aucun état sémantique : elle applique les événements au DOM et renvoie les entrées utilisateur.

Composants côté Rust :

- `pty` : spawn du shell via `portable-pty`, lecture dans un thread dédié, écriture des entrées, propagation du resize (`TIOCSWINSZ`).
- `term` : état du terminal classique via `alacritty_terminal` (grille, scrollback, modes, écran alternatif, curseur, attributs). On ne réécrit pas de parseur ANSI.
- `rich` : filtre placé avant le parseur ANSI, qui détecte et extrait la séquence OSC propriétaire (voir section suivante) et produit des blocs riches.
- `view` : fenêtre `tao` + webview `wry`, page HTML/CSS/JS embarquée dans le binaire par `include_str!`, communication par `ipc` handler de wry (webview → Rust) et `evaluate_script` (Rust → webview).
- Ligne de commande, compatible avec alacritty : `-T`/`--title` (titre initial de la fenêtre, auquel un reset de titre revient) et `-e`/`--command` (programme et arguments lancés à la place du shell ; prend tous les arguments qui suivent).
- `config` : fichier TOML dans `$XDG_CONFIG_HOME/rt/config.toml` (shell, police, taille du scrollback, curseur, titre dynamique, gras en couleurs vives, décorations, raccourcis). Les couleurs et tout CSS additionnel vont dans `user.css`, à côté.

Modèle d'affichage : la sortie est un flux vertical de blocs. En écran principal, chaque ligne qui sort de la grille par le haut devient un élément DOM statique dans le scrollback, et un bloc riche est inséré dans ce flux à la position du curseur, comme une ligne de hauteur variable. En écran alternatif, la webview affiche une grille fixe de cellules et masque le flux, puis le restaure à la sortie. Cette distinction est la décision structurante du projet : elle permet un scrollback DOM natif (sélection, recherche, zoom) sans sacrifier les TUI.

Option gardée ouverte : séparer plus tard en client/serveur (le cœur sert une page sur localhost, le frontend est un navigateur ou l'outil webapp existant). Le protocole IPC doit donc être sérialisable et ne pas dépendre d'API wry spécifiques.

## Protocole de sortie riche

Une seule séquence OSC propriétaire, numéro `7117` (hors des plages utilisées par xterm, iTerm2 et kitty), avec un payload en base64 pour ne jamais avoir à échapper le contenu :

```
ESC ] 7117 ; <kind> [; <k>=<v> ...] ; <base64> BEL
```

`ST` (`ESC \`) est accepté comme terminateur à la place de `BEL`. Les clés optionnelles sont `w` (largeur max en colonnes), `h` (hauteur max en lignes), `id` (identifiant pour remplacer un bloc existant). Le payload base64 est limité à 8 Mo ; au-delà, le bloc est ignoré et un avertissement est journalisé.

| kind | payload | rendu |
| --- | --- | --- |
| `html` | fragment HTML | inséré tel quel dans un conteneur isolé (voir sécurité) |
| `md` | Markdown (CommonMark + tables GFM) | converti en HTML côté Rust avec `comrak`, puis comme `html` |
| `svg` | document SVG | inséré inline, dimensionné à la largeur du terminal si pas de `w` |
| `img` | `mime=<type>` + octets | balise `<img>` avec data URL, redimensionnée à la largeur du terminal |
| `video` | `mime=<type>` + octets, ou `src=path` + chemin absolu d'un fichier local | balise `<video>` avec contrôles ; un fichier local est servi par le serveur média de `rt` (voir sécurité) |

Sémantique :

- Le bloc est inséré à la position du curseur, comme une ligne de hauteur variable. Le curseur passe à la ligne suivante après le bloc.
- En écran alternatif, les blocs sont ignorés (pas de rendu riche dans vim).
- Un bloc avec `id` remplace le bloc précédent de même `id` s'il existe, sinon il est inséré normalement. Permet les barres de progression et les tableaux qui se remplissent.
- Les blocs `img` et `svg` sont limités en hauteur par `[images] max_height` (en lignes) dans la config, en plus de leur clé `h`. Un clic sur l'un d'eux l'affiche en plein écran sur fond sombre ; Échap, la croix en haut à droite ou un clic n'importe où ramènent au terminal.
- Le bloc fait partie du scrollback : il est sélectionnable, copiable (comme texte brut pour `md`, comme image pour `img`/`svg`) et conservé dans l'export de session.

Sécurité : le HTML n'est pas exécuté dans la page du terminal elle-même. Chaque bloc `html`/`md`/`svg` est rendu dans un `<iframe sandbox>` sans `allow-scripts`, avec `srcdoc` et une feuille de style injectée, redimensionné à la hauteur de son contenu via `ResizeObserver` dans la page parente. Un programme qui écrit dans le terminal ne doit jamais pouvoir exécuter du JavaScript dans le processus `rt`. Les `<a href>` sont ouverts via `xdg-open` par le cœur Rust, jamais dans la webview.

Vidéos : le lecteur de WebKitGTK passe par GStreamer (il faut `gst-plugins-good`, et `gst-libav` pour H.264) et ne lit que des URL http(s), `data:` ou `blob:`, ni `rt://` ni `file://`. Pour lire un fichier local de toute taille avec avance rapide, `rt` lance un petit serveur HTTP sur `127.0.0.1` (port aléatoire) qui ne sert que les fichiers annoncés par un bloc `video` avec `src=path`, par index, sous un jeton aléatoire de 128 bits, avec les requêtes `Range`. La CSP n'autorise que ce serveur comme source de médias. `rat` envoie le chemin pour un fichier et le contenu en base64 depuis stdin. En ssh (`SSH_CONNECTION`), il refuse d'émettre une vidéo : le chemin désignerait un fichier distant que `rt` ne peut pas lire, et le contenu entier ferait transiter des mégaoctets par la connexion. `rt`, lui, affiche toute vidéo qu'il reçoit.

Détection côté programme : `rt` exporte `RT_TERM=1` et `TERM_PROGRAM=rt` dans l'environnement du shell. `rat` et tout script compatible testent cette variable et se replient sur du texte brut sinon. `TERM` reste `xterm-256color` pour que les programmes existants fonctionnent.

## Émulation du terminal classique

L'état du terminal est entièrement délégué à la crate `alacritty_terminal` : on l'instancie avec un `Term`, on lui donne les octets du pty via son `Processor`, et on lit la grille, le curseur, les modes et le scrollback. Ça couvre d'office les couleurs 16/256/24 bits, les attributs (gras, italique, souligné, inversé), l'écran alternatif, les modes de curseur, le bracketed paste, la souris et les séquences de titre. On n'écrit pas de parseur maison.

Ce que le cœur fait par-dessus :

- Filtre OSC 7117 avant le `Processor` : les octets de la séquence propriétaire ne doivent jamais atteindre la grille.
- Diff de grille : après chaque lot d'octets, calcul des lignes modifiées (`alacritty_terminal` expose les lignes damaged) et émission vers la webview d'un événement par lot : `{"type":"lines","rows":[{"y":12,"cells":[...]}],"cursor":{...}}`. Chaque cellule est sérialisée en runs (texte + style) et non cellule par cellule.
- Scrollback : quand une ligne sort de la grille par le haut en écran principal, elle est envoyée une seule fois comme `{"type":"scroll_out","line":...}` et devient DOM statique. Le scrollback vit dans la webview, pas dans `alacritty_terminal` (configurer sa `history_size` à 0 ou minimale).
- Écran alternatif : événement `{"type":"alt","on":true}` ; la webview bascule sur la grille fixe. Retour avec `on:false`.
- Taille : la webview calcule les dimensions d'une cellule (`measureText` sur la police mono configurée), déduit `cols × rows` à chaque resize et l'envoie au cœur, qui redimensionne le pty et le `Term`.
- Entrée : la webview capture `keydown`, encode en séquences (flèches, F1-F12, Ctrl, Alt, modes application) et envoie au pty. Le coller passe par un événement dédié pour respecter le bracketed paste.

Tests : un jeu de fichiers `.in` (flux d'octets enregistrés via `script` sur de vraies sessions : `ls --color`, `cargo build`, `git diff`, `htop` quelques secondes, `vim` ouvrir/quitter) avec des snapshots `.out` de la grille finale et de la liste d'événements émis. `cargo test` doit tourner sans fenêtre ni webview : le cœur est testable en isolation, c'est une contrainte d'architecture, pas un détail.

## Frontend web

Une page HTML, un fichier CSS, un fichier JS, en vanilla, sans framework, sans bundler, sans Node. Les trois sont embarqués dans le binaire et servis à la webview via un protocole custom wry (`rt://app/`). Le CSS utilisateur (`$XDG_CONFIG_HOME/rt/user.css`) est chargé après, pour que tout soit surchargeable.

Structure du DOM :

```
<main id="flow">          flux : scrollback + blocs riches + grille courante
  <div class="line">…    une ligne sortie de la grille (spans stylés)
  <div class="rich">…    un bloc riche (iframe sandbox)
  <div id="screen">…      la grille courante, rows × cols de <div class="line">
<main id="alt" hidden>   grille fixe pour l'écran alternatif
```

- Police mono configurable, taille de cellule mesurée une fois par changement de police ou de zoom (`Ctrl +`/`Ctrl -`), renvoyée au cœur.
- Couleurs via variables CSS (`--fg`, `--bg`, `--c0` à `--c15`, `--cursor`, `--selection`), thème par défaut sombre, surcharge dans `user.css`.
- Sélection : sélection native du navigateur dans le flux, ce qui donne gratuitement la sélection multi-lignes, le double-clic sur un mot et le copier dans les blocs riches. `Ctrl+Shift+C` copie, `Ctrl+Shift+V` colle. `Shift+Insert` et le clic milieu collent la sélection primaire (lue côté Rust via GTK, le JS n'y a pas accès). La grille de l'écran alternatif utilise la même mécanique.
- Raccourcis : une table `[keys]` associe une combinaison (`"ctrl+shift+e"`) à une action (`copy`, `paste`, `paste_selection`, `search`, `zoom_in`, `zoom_out`, `zoom_reset`, `scroll_page_up`, `scroll_page_down`, `new_window`, `none` pour retirer un défaut) ou à du texte envoyé au programme (`{ send = "..." }`). `new_window` lance une nouvelle fenêtre `rt` dans le répertoire courant du shell.
- Scroll : le flux scrolle nativement. Tant que l'utilisateur n'est pas en bas, les nouvelles sorties ne forcent pas le défilement ; une frappe clavier ramène en bas. `Shift+PageUp/PageDown` pour le clavier.
- Recherche : `Ctrl+Shift+F` ouvre une barre de recherche qui utilise `window.find` ou un parcours DOM simple avec surlignage.
- Curseur : bloc, barre ou souligné selon la config, clignotement désactivable.
- Performance : rendu par lots via `requestAnimationFrame`, jamais une mise à jour DOM par octet. Le scrollback est plafonné (défaut 10 000 lignes) ; au-delà, les plus anciennes sont retirées du DOM.
- Titre de fenêtre : suit OSC 0/2 (titre envoyé par le shell).

Interaction webview → Rust via `window.ipc.postMessage(JSON)` : `{"type":"input","data":"..."}`, `{"type":"resize","cols":..,"rows":..}`, `{"type":"paste","data":"..."}`, `{"type":"open_url","url":"..."}`. Le JS ne doit jamais accéder au réseau ni au système de fichiers.

## Outil CLI compagnon : rat

Second binaire du même workspace Cargo, sans dépendance à wry. Il lit un fichier ou stdin, choisit le `kind` d'après l'extension ou un flag, encode et écrit la séquence OSC 7117 sur stdout. Hors de `rt` (pas de `RT_TERM`), il se replie sur un affichage texte raisonnable au lieu de polluer le terminal.

| commande | kind émis | repli hors rt |
| --- | --- | --- |
| `rat fichier.md` ou `rat --md -` | `md` | le Markdown brut |
| `rat page.html` ou `--html` | `html` | le HTML brut |
| `rat image.png` (png, jpg, gif, webp) | `img` | le chemin du fichier |
| `rat video.mp4` (mp4, webm, mkv, ogv) | `video` | le chemin du fichier |
| `rat dessin.svg` | `svg` | le chemin du fichier |

Options : `--width N`, `--height N`, `--id NOM` (mappées sur les clés du protocole), `--force` pour émettre la séquence même sans `RT_TERM`.

Utilisation visée :

```
rat rapport.md
cargo bench | rt-plot | rat --svg -      # un futur outil émet du SVG
curl -s https://… | rat --html -
```

La conversion Markdown se fait côté `rt` et non dans `rat` : ça garde `rat` minuscule et permet à n'importe quel script shell d'émettre `md` avec un `printf` et `base64`, sans dépendre du binaire. Documenter cette recette dans le README (dix lignes de bash).

## Jalons

Chaque jalon est livrable et testable seul. Claude Code travaille un jalon à la fois, ouvre une branche par jalon, et s'arrête pour validation avant le suivant.

1. **M0 : la plomberie.** Workspace Cargo avec les deux crates, fenêtre `tao` + webview `wry` qui affiche une page embarquée, pty qui lance `$SHELL`, les octets bruts affichés dans un `<pre>` sans aucune interprétation, les frappes renvoyées au pty. Critère : taper `ls` et voir le résultat, même moche.
2. **M1 : un vrai terminal.** Intégration d'`alacritty_terminal`, diff de grille, scrollback DOM, écran alternatif, resize, couleurs, sélection et copier-coller. Tests snapshot en place. Critère : une journée de travail dans `rt` sans ouvrir un autre terminal : cargo, git, vim, htop, less, `ssh` vers une machine distante.
3. **M2 : la sortie riche.** Filtre OSC 7117, les quatre kinds, l'iframe sandbox, `rat`, le remplacement par `id`. Critère : `rat rapport.md` affiche un tableau large lisible avec lignes alternées, `rat image.png` affiche l'image, un `<script>` dans un bloc `html` ne s'exécute pas.
4. **M3 : confort.** Config TOML, `user.css`, zoom, recherche, liens cliquables (OSC 8 et URL détectées), titre de fenêtre, plafond de scrollback, raccourcis configurables. Critère : remplacer le terminal par défaut sur le poste.
5. **M4 : packaging.** Binaire statique ou `.deb`/AUR, fichier `.desktop`, README avec la recette bash pour émettre un bloc sans `rat`, export de session en HTML autonome.

M2 est le cœur de la valeur ; si M1 prend trop de temps, on accepte un M1 dégradé (pas de souris, pas de sélection dans l'écran alternatif) pour atteindre M2 plus vite.

## Contraintes techniques et conventions

- Rust stable, édition 2024, `cargo clippy -- -D warnings` et `cargo fmt --check` propres à chaque commit.
- Crates imposées : `wry` + `tao` (fenêtre et webview, déjà utilisées dans le projet webapp), `portable-pty`, `alacritty_terminal`, `comrak` (Markdown, avec l'extension `table`), `base64`, `serde`/`serde_json`, `clap` pour `rat` et les options de `rt`, `toml` pour la config, `gtk` pour la sélection primaire (déjà tirée par `tao`, même version). Pas d'autre dépendance sans justification dans le commit.
- Cible : Linux uniquement, X11 et Wayland via WebKitGTK (ce que wry utilise sur Linux). Pas de code spécifique à macOS ou Windows, pas de `cfg` morts.
- Frontend : HTML/CSS/JS vanilla, un seul fichier par type, pas de Node, pas de bundler, pas de dépendance JS externe. Pas de `eval`, pas de `innerHTML` avec du contenu venant du pty en dehors de l'iframe sandbox.
- Le cœur (`pty`, `term`, `rich`) est une bibliothèque testable sans fenêtre ; `view` est le seul module qui connaît wry. `cargo test` passe en CI headless.
- Licence AGPL-3.0, dépôt GitHub, README en anglais, commentaires de code en anglais.
- Commits atomiques, messages à l'impératif, sans trailer (pas de Co-Authored-By, pas de mention de génération).
- Pas de fonctionnalité ajoutée au-delà de la spec sans l'écrire d'abord ici. Un doute sur une décision structurante : s'arrêter et demander, ne pas trancher seul.

## Hors périmètre v1

- Onglets, splits, multiplexage : tmux fait ça très bien dans `rt`.
- Mode client/serveur et accès distant par navigateur (l'architecture le permet, on ne le construit pas).
- Protocoles graphiques kitty et Sixel : un programme qui veut afficher une image dans `rt` passe par OSC 7117. Une passerelle Sixel → `img` pourrait venir en v2.
- Ligatures, rendu GPU, performances Alacritty : la webview est acceptée comme plafond.
- macOS et Windows.
- Exécution de JavaScript dans les blocs riches, widgets interactifs renvoyant des événements au programme : la v1 est en lecture seule, c'est une décision de sécurité autant que de périmètre.
- Thèmes prédéfinis au-delà du sombre par défaut : `user.css` suffit.
