# brAIn — como usar

O brAIn mostra a árvore de pastas de `~/workspace` como um grafo e abre o Claude Code
(`claude` ou `claude --resume <id>`) no terminal que você escolher, na pasta certa e com o
contexto certo.

## 1. Instalar e abrir
- RPM: `cd` até a pasta do projeto e `sudo dnf install ./src-tauri/target/release/bundle/rpm/brAIn-0.1.0-1.x86_64.rpm`.
- AppImage: `chmod +x brAIn_0.1.0_amd64.AppImage && ./brAIn_0.1.0_amd64.AppImage`.
- Abra pelo menu de aplicativos. O `claude` precisa estar instalado (`~/.local/bin/claude`);
  o caminho pode ser trocado em Configurações → Terminal.

## 2. Tela principal
| Área | Função |
|---|---|
| Sidebar | Árvore de pastas, busca e chips de categoria. Clique em ▸ para expandir. |
| Grafo | Cada pasta é um nó; a cor é a categoria; o tamanho reflete a atividade recente. |
| Painel (direita) | Detalhes da pasta selecionada: categoria, bases de contexto e pensamentos (sessões). |

## 3. Abrir o Claude Code
- **Novo pensamento**: abre `claude` na pasta selecionada.
- **Retomar pensamento**: escolha uma sessão na lista e retome (ou dê duplo clique nela).
- Ao abrir, o brAIn injeta as **bases de contexto** que valem para aquela pasta.

## 4. Categorias
Seis categorias padrão, atribuíveis a qualquer pasta e **herdadas** pelas subpastas:

| Categoria | Tipo | Acesso do Claude |
|---|---|---|
| Projetos, Educação, Clientes | consumidoras (recebem contexto) | — |
| Documentações | fonte | só leitura |
| Agentes | fonte | só leitura |
| Diário | fonte | leitura + escrita incremental (pede confirmação a cada edição) |

Atribua pelo painel da pasta. Nome, cor e descrição são editáveis em Configurações → Categorias.

## 5. Bases de contexto
Uma base é uma pasta ou arquivo que o Claude passa a poder ler ao ser aberto em outra pasta.
Em Configurações → Contexto você escolhe, numa matriz, **quais bases valem para quais
categorias ou pastas**. Botões: **+ Pasta**, **+ Arquivo**, **+ Agente**, **Sincronizar agentes**.

- Leitura: o brAIn bloqueia edição na base (`deny Edit`).
- Escrita incremental (Diário): cada edição pede confirmação (`ask Edit`).
- Limite de 20 bases por lançamento; excedentes aparecem como ignoradas (⚠) com o motivo.
- Pastas amplas (`~`, `/home`) e sensíveis (`~/.ssh`, `~/.config`, `~/.claude`…) são recusadas.
- **Limite honesto:** a regra de somente leitura cobre as ferramentas de arquivo e os
  redirecionamentos do Bash, mas não interpretadores arbitrários (`python -c`).

### Agentes
O brAIn nunca toca em `~/.claude`. Ele copia `~/.claude/agents/*.md` para `~/.claude_copy/agents`
(ao abrir, antes de cada lançamento e pelo botão "Sincronizar agentes"). A cópia é descartável e
nunca apaga nada; arquivos removidos na origem aparecem como "obsoletos".

## 6. Atalhos
| Atalho | Ação |
|---|---|
| Ctrl+K | Paleta: buscar pasta ou executar ação |
| Ctrl+, | Configurações |
| Esc | Fechar paleta/modal |

## 7. Configurações
Abas **Pastas** (raízes, profundidade, ignorados), **Terminal** (Ptyxis, GNOME Terminal ou
comando próprio com `{command}`), **Categorias** e **Contexto**. Nada é gravado até clicar em
**Salvar**. Arquivos: `~/.config/brain/config.json`; cache em `~/.cache/brain/`.

## 8. Problemas comuns
- *Nada abre*: confira o terminal e o caminho do `claude` em Configurações → Terminal.
- *Base com ⚠*: leia o motivo no painel (pasta ampla, proibida, contém o projeto, limite de 20).
- *Config corrompida*: o app recupera os padrões e avisa; o arquivo antigo é preservado.
- *Pasta não aparece*: pastas que começam com `.` e as da lista "ignorar" são puladas; o teto é 5000 nós.
