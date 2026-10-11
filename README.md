# brAIn

Launcher visual de projetos para o **Claude Code**. O brAIn mostra a árvore de pastas do seu workspace como um grafo e abre o `claude` (novo ou retomando uma sessão) no terminal, na pasta certa, já com o contexto que você escolheu.

Tauri 2 (Rust) + React/TypeScript + Vite + `force-graph`. Alvo: Fedora (GNOME/Wayland). Tema escuro, sem rede: o app só lê arquivos locais.

## O que faz

- **Grafo de pastas**: nó central em `~/workspace`, arestas pai→filho, expandir/recolher por duplo clique, modo Foco e busca sobre toda a árvore.
- **Novo pensamento / Retomar pensamento**: abre `claude` ou `claude --resume <id>` em Ptyxis, GNOME Terminal ou um terminal personalizado. Os comandos são montados como lista de argumentos, nunca por shell.
- **Categorias**: Projetos, Educação, Clientes (consumidoras) e Documentações, Agentes, Diário (fontes). Atribuídas a qualquer pasta e herdadas pelas subpastas. Editáveis em Configurações.
- **Bases de contexto**: arquivos ou pastas que o Claude pode consultar ao abrir uma pasta, ligadas a pastas específicas ou a categorias inteiras (matriz em Configurações > Contexto).

## Acesso às bases de contexto

O acesso vem da categoria da fonte e é imposto no lançamento via `--settings` do Claude Code:

| Categoria da fonte | Acesso |
|---|---|
| Documentações, Agentes (e qualquer base fora de categoria-fonte) | somente leitura |
| Diário | pode criar arquivos novos; editar um existente pede confirmação |

- Base **pasta**: `--add-dir` + regra `deny`/`ask` de `Edit` sobre a pasta.
- Base **arquivo**: libera só aquele arquivo (`Read`), sem conceder a pasta que o contém.
- Pastas sensíveis (`~/.ssh`, `~/.claude`, `~/.config`, `~/.gnupg`, `/etc`…) nunca são concedidas; pastas muito amplas (`$HOME`) só entram no texto do prompt.

**Limite conhecido:** a regra de somente leitura cobre as ferramentas de arquivo e redirecionamentos do Bash. Um interpretador arbitrário (por exemplo `python -c`) não é barrado pela regra, só pela aprovação de comandos do Claude Code e pelo texto do prompt. Em fontes sensíveis, não libere Bash de forma ampla.

### Agentes

`~/.claude` é bloqueado de propósito (contém credenciais e sessões). Os agentes são copiados de `~/.claude/agents` para `~/.claude_copy/agents` ao abrir o app, antes de cada lançamento e pelo botão "Sincronizar agentes". A cópia é **descartável**: edite sempre a origem. Nada é apagado na cópia; arquivos que sumiram da origem são apenas reportados.

## Configuração e dados

- `~/.config/brain/config.json` (permissão `0600`; `.bak` automático quando há recuperação): raízes, profundidade, pastas ignoradas, terminal, categorias, atribuições e bases. Tudo isso é editável pela tela de Configurações (`Ctrl+,`).
- `~/.cache/brain/`: cache do índice de sessões (por caminho, mtime e tamanho).
- Sessões são lidas de `~/.claude/projects/*/*.jsonl` (formato interno do Claude Code, que pode mudar; arquivos ilegíveis aparecem como sessões "parciais" e nunca derrubam o app).

### Terminal personalizado

Informe o executável (caminho absoluto) e os argumentos, um por linha. Uma das linhas precisa ser exatamente `{command}`: ela é substituída pelo restante do argv (`claude …`). O diretório de trabalho é aplicado pelo brAIn. Exemplo: `-e` e `{command}`.

## Atalhos

| Atalho | Ação |
|---|---|
| `Ctrl+K` | focar a busca |
| `Ctrl+,` | Configurações |
| `Esc` | limpar busca / fechar painel |
| Setas, `Enter` | navegar e expandir a árvore na sidebar |

## Desenvolvimento no Fedora

Dependências do sistema e Rust:

```bash
sudo dnf install -y webkit2gtk4.1-devel openssl-devel curl wget file libappindicator-gtk3-devel librsvg2-devel libxdo-devel
sudo dnf group install -y "development-tools"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

Node 20+ e npm também são necessários.

```bash
npm install
npm run tauri dev        # app em modo desenvolvimento
npx tsc --noEmit         # checagem de tipos do front
cd src-tauri
cargo clippy --all-targets -- -D warnings
cargo test               # unitários + adversariais (dados sintéticos)
```

## Empacotamento

```bash
sudo dnf install -y rpm-build     # necessário só para o RPM
npm run tauri build               # gera AppImage e RPM
# ou um formato só:
npm run tauri build -- --bundles appimage
npm run tauri build -- --bundles rpm
```

Os artefatos ficam em `src-tauri/target/release/bundle/`. Ao abrir pelo GNOME (e não pelo terminal), o `PATH` é reduzido: o brAIn resolve o `claude` para um caminho absoluto (`~/.local/bin/claude` e afins) e aceita um caminho manual em Configurações > Terminal.

## Estrutura

```
src/                 React: App, Settings, graph/FolderGraph, lib/{api,tree}
src-tauri/src/       Rust: config, tree, sessions, bases, launcher, agents_sync, commands
src-tauri/tests/     testes adversariais e fixtures sintéticas
assets/icon.svg      ícone do app (fonte dos PNGs em src-tauri/icons)
```

## Licença

Apache-2.0 — veja [LICENSE](LICENSE) e [NOTICE](NOTICE).
