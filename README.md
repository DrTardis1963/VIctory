# VIctory

> Tradução do README original em inglês: [README.en.md](README.en.md).

Executa Grand Theft Auto VI (Xbox Series X|S) nativamente no Windows, a partir
de arquivos fornecidos pelo usuário.

O código x86-64 do título roda sem modificações dentro do nosso processo; o
VIctory fornece a plataforma por baixo dele e converte os dados do título,
antecipadamente quando possível e no primeiro uso quando não há outro jeito
(shaders). Veja [docs/architecture.md](docs/architecture.md).

O executável do Windows também pode ser iniciado com o Proton em Linux x86-64.
Veja [Running with the Proton launcher](docs/proton-launcher.md).

## Progresso na NVIDIA (este fork)

<!-- progress:start -->
![shaders: 88.8%](https://img.shields.io/badge/shaders-88.8%25-yellow)
![Mesh shaders: 97.5%](https://img.shields.io/badge/Mesh%20shaders-97.5%25-green)
![Vertex/pixel shaders: 88.8%](https://img.shields.io/badge/Vertex%2Fpixel%20shaders-88.8%25-yellow)
![Compute shaders: 59.9%](https://img.shields.io/badge/Compute%20shaders-59.9%25-orange)
![measured: 2026-11-24](https://img.shields.io/badge/measured-2026--11--24-blue)

| Part | Progress | % | Count |
|---|---|---|---|
| **All shaders** | `██████████████████░░` | **88.8%** | 22519 / 25334 |
| Mesh shaders | `████████████████████` | 97.5% | 8501 / 8713 |
| Vertex/pixel shaders | `██████████████████░░` | 88.8% | 12469 / 14039 |
| Compute shaders | `████████████░░░░░░░░` | 59.9% | 1549 / 2582 |
| **Game stages working** | `██████████░░░░░░░░░░` | **50.0%** | 3 / 6 |

| Game stage | State | Measured |
|---|---|---|
| Boot and first-boot screens | ✅ | 30 fps |
| Intro video with sound | ✅ | 30 fps, 33.3 ms per frame |
| Start screen and menus | ✅ | 30 fps |
| First loading screen | ⚠️ | 1-2 fps; ~2.5 min without a frame while the driver makes the world's pipelines (cold cache) |
| First cutscene | ⚠️ | reached; 90-160 s per frame; some runs lose the device after 2-4 min |
| Free roam | ❌ | not reached |

*Measured 2026-11-24 on NVIDIA GeForce RTX 4060 Ti 8 GB, Intel Core i7-13700K, Windows 11 (`d3a24ce9`). Shader counts are the console shaders of the Xbox Series 1.0.2.1 dump that translate for this host (`victory translate`); game stages are what a run reached. Regenerate with `cargo xtask progress` after editing `docs/progress/progress.json`.*
<!-- progress:end -->

## Situação (2026-11-23)

**Ainda não é jogável.** O título inicia, roda seus menus nos 30 fps dele e
carrega o mundo, onde a renderização falha.

| Etapa | Estado |
|-------|--------|
| Inicialização, telas da primeira execução (acessibilidade, idioma, configurações, aviso legal) | funciona, 30 fps |
| Vídeo de abertura com som | funciona |
| Tela inicial, perfil e saves | funciona |
| Carregamento do mundo (cerca de 50 s) | funciona |
| O mundo (abertura da primeira missão) | alcançado e roda com draws indiretos; o cartão de título é renderizado, a cena seguinte ainda fica escura; 0,5-1,8 s por quadro |

Testado em uma única máquina: Windows 11, AMD RX 6700 XT.

> **Aviso.** Entrar no mundo em uma GPU real pode, no momento, travá-la, e o
> Windows então reinicia o driver de vídeo (todos os monitores ficam pretos por
> um instante). Para investigar mais, use um build de diagnóstico com
> `VICTORY_D3D12_WARP=1`, que renderiza no adaptador por software.

O que cada parte faz hoje e o que ainda falta:

| Subsistema | Hoje | Falta |
|------------|------|-------|
| Loader | Executa o executável do título e suas DLLs dentro do nosso processo; os relatórios de falha apontam o código que falhou e são guardados em `crashes.log` | – |
| Serviços de runtime | Um usuário offline logado, licenças, pacotes, saves que sobrevivem a uma interrupção em qualquer etapa, diálogo de entrada de texto | A fala é muda; o diálogo e a reinicialização não foram testados ao vivo |
| Rede | Offline por projeto: o título não consegue acessar a rede | – |
| Arquivos e streaming | Arquivos e dados em streaming lidos, zlib e BCPack decodificados em paralelo | O carregamento é limitado pelas escritas na memória do título |
| Entrada | Controles XInput, teclado, controle simulado pelo teclado, vibração | – |
| Áudio | Som de menu, vídeo e carregamento; vozes XMA e streams Opus são decodificados | O reverb é mudo; resultados de Opus não verificados contra o título |
| GPU: menus | Draws, compute e apresentação no D3D12 do host; só a memória alterada do título é copiada | O ritmo de quadros oscila em uma tela nos builds de release |
| GPU: mundo | Pipelines de mesh, buffers escritos por shaders, tempo de vida de descritores, execução indireta (argumentos limitados), pipelines criados por threads de trabalho e mantidos em disco, depth bounds em adaptadores compatíveis (testes sintéticos) | A maior parte da cena ainda falta (shaders não traduzidos descartam draws); os quadros levam segundos; faltam queries reais, HDR e escalonamento |
| Shaders | Código de GPU do console recompilado para DXIL no primeiro uso, com cache em disco. Menus: todos. Mundo: mesh 8.191 de 8.506, vertex/pixel 10.617 de 12.346, compute 1.643 de 2.539 | Estágios de geometria mesclados, shaders do construtor de ray tracing; alguns programas de compute precisam de uma GPU com ondas de 64 lanes |
| Texturas | Layouts em tiles do console e destiling para os modos que o título usa | Planos de metadados de render target |
| Modo Series S | Roda pelos menus até a tela de carregamento | O mundo, como acima |

Em uma varredura offline de `shaders.rpf`, os atomics de textura do SM 6.6
recuperaram 520 traduções de shaders. A especialização de resultados de
desvios comprovados recuperou outras 108, mantendo palavras de descritores
fora dos caminhos que consomem dados de buffer. O validador DXIL da Microsoft
aceitou todos os 628 shaders. A renderização do jogo com essas mudanças de
shader ainda precisa de uma execução em D3D12 no Windows.

## Primeiros passos

Requisitos: Windows 10/11 x64, uma CPU com AVX2, uma GPU Direct3D 12 com
shader model 6.6 (e mesh shaders para o mundo), Rust (versão fixada em
`rust-toolchain.toml`). No Linux, veja
[docs/linux-proton.md](docs/linux-proton.md) para saber quais crates compilam
e são testados nativamente hoje, como fazer a compilação cruzada do
`victory.exe` e como rodá-lo com o Proton.

No Wine ou no Proton, a execução indireta também precisa do `dxcompiler.dll`
do DXC (com o seu `dxil.dll`): o `d3dcompiler_47` do Wine não consegue compilar
o limitador dos argumentos indiretos. Ele é procurado em `VICTORY_DXC` (a DLL
ou a pasta dela), ao lado do `dxil.dll` que a validação de shaders encontra,
depois na pasta do executável e em System32.

O título também precisa de vários GiB de commit disponível do Windows na
inicialização. Confira **Confirmado** em **Gerenciador de Tarefas → Desempenho
→ Memória**. Se o `crashes.log` informar o erro 1455 do Windows, feche outros
aplicativos ou aumente o tamanho do arquivo de paginação do Windows e tente de
novo.

Mantenha a pasta do jogo em um SSD, se puder. Em um disco rígido, a primeira
tela fica preta, a 30 fps e cerca de 3 draws/quadro, por um minuto e meio ou
mais enquanto o título lê seus arquivos: o disco fica 100% ocupado com
pequenas leituras aleatórias, e nada aparece até terminarem. Espere, em vez de
fechar a janela. Uma execução posterior é mais rápida enquanto o Windows ainda
mantém os arquivos no cache.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Execute o título, trocando o caminho de exemplo pelo caminho completo do
executável do seu jogo:

```powershell
cargo build --release -p victory
.\target\release\victory.exe run 'D:\Xbox Games\GTA VI\title.exe' --execute
```

### Selecionando o perfil Series S

O GTA VI escolhe sua configuração de renderização pelo modelo do console, não
pelos modos de vídeo que o VIctory anuncia pelo DXGI. Para usar o perfil de
resolução mais baixa, passe `--console series-s` ao iniciar o título:

```powershell
target\release\victory.exe run <game dir>\<the title's exe> --execute --console series-s
```

Em uma execução de diagnóstico relatada, o perfil Series X usou uma cena de
3840×2160 com alvos internos de 2560×1440; o perfil Series S usou uma cena de
1920×1080 com alvos internos de 1280×720. A interface ainda pode usar um plano
de apresentação separado de 3840×2160, então selecionar Series S não torna
necessariamente a janela ou todos os render targets 1080p. Alterar apenas a
lista de modos do DXGI não muda a resolução da cena
([issue #203](https://github.com/Marvisak/VIctory/issues/203)); a
apresentação multiplano do Series S é tratada separadamente no
[PR #211](https://github.com/Marvisak/VIctory/pull/211).

O mesmo relatório mediu cerca de 3,9 GB a menos de memória confirmada pelo
processo nos menus/abertura e cerca de 3,6 GB a menos no pico do mundo com o
Series S ativado. Mesmo assim, o pico de memória confirmada ainda foi de cerca
de 17,7 GB, e o uso de memória de GPU do sistema inteiro ainda chegou a cerca
de 7,3 dos 8 GB. Os dois perfis também perderam o dispositivo de GPU de 30 a
45 segundos depois de entrar no mundo. São medições de uma única configuração
de diagnóstico (Windows 10 / RTX 2070 SUPER 8 GB / 32 GB de RAM, commit base
`d2a4bae`, com os PRs #175 e #211 aplicados), não requisitos gerais nem
garantia de estabilidade. O Series S pode reduzir a pressão sobre a memória,
mas não faz um sistema com 16 GB de RAM / 8 GB de VRAM comportar o mundo de
forma confiável nem resolve o travamento atual da GPU.
A legenda da janela mostra a taxa de quadros. Saves, configurações e o
`crashes.log` ficam na pasta de dados (padrão `%LOCALAPPDATA%\victory\data`),
e os dados derivados ficam na pasta de cache.

Entrada: controles XInput. Um DualSense ou DualSense Edge (USB ou Bluetooth)
também é lido por HID e funciona como o primeiro controle quando nenhum
controle XInput responde. Sem nenhum controle conectado, o teclado funciona
como controle enquanto a janela do título tem o foco:

| Teclas | Controle |
|--------|----------|
| W A S D / I J K L | analógico esquerdo / direito |
| setas | direcional (D-pad) |
| Enter, O | A, B |
| Backspace, X, Y | B, X, Y |
| Q / E, Z / C | botões de ombro, gatilhos |
| F / H | cliques dos analógicos |
| Esc, Tab | Menu, View |

## Comandos

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logs: `VICTORY_LOG=victory_loader=trace,info` (um build de release registra
info e acima).

Para relatórios de falha estruturados, veja
[Dependency diagnostics](docs/dependency-diagnostics.md).

## Depuração

Um build de release não tem recursos de depuração. Inclua-os com

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variável | Efeito |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | saída de debug e trace |
| `VICTORY_STACKS=20,60` | a pilha de cada thread nesses segundos |
| `VICTORY_WATCH=<rva>,...` | registra um u32 do título quando ele muda |
| `VICTORY_KEYS=30:Enter,35:X` | pressiona teclas nesses segundos, com ou sem foco |
| `VICTORY_CRASH_BREAKPOINTS=1` | acrescenta breakpoints de primeira chance ao `crashes.log`, inclusive os que um handler pode recuperar |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | salva memória do título na pasta de cache em caso de falha |
| `VICTORY_CAPTURE=<dir>` | salva os quadros apresentados (potências de dois, ou `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | junto com os dois acima, salva todos os render targets desses quadros |
| `VICTORY_D3D12_DEBUG=1` | camada de debug do host |
| `VICTORY_D3D12_GBV=1` | junto com o anterior, validação baseada em GPU (uso incorreto de descritor é reportado, não gera falha; lento) |
| `VICTORY_D3D12_DRED=1` | em uma falha de GPU, a alocação atingida e os comandos em andamento |
| `VICTORY_FPS=60` | um quadro no máximo a cada 1/60 s quando o título pede menos (a abertura do Grand Theft Auto VI pede 30); funciona também em builds de release |
| `VICTORY_DYNAMIC_SLOTS=<n>` | em um host cujo heap de views é limitado a 1.000.000 (NVIDIA), `n` dos seus slots (4096 a 500000, padrão 65536) guardam as tabelas que os draws criam do zero e o restante espelha os primeiros slots do título; a linha `dynamic descriptor slots`, impressa a cada 5 segundos, informa quantos foram usados no máximo e quantas tabelas não puderam ser criadas |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omite draws e dispatches, inclusive os indiretos, cujo pipeline contém um hash de shader de 16 dígitos listado |
| `VICTORY_SKIP_WAVE64=mesh,compute` | omite draws e dispatches cujo pipeline tem um programa de um estágio listado escrito para ondas de 64 lanes (em hosts com ondas de 32 lanes, como os da NVIDIA, eles podem travar a GPU) |
| `VICTORY_GPU_TIMING=1` | junto com o anterior, cada lista de uma submissão é executada e cronometrada isoladamente; listas lentas são registradas com seus comandos mais lentos |
| `VICTORY_D3D12_WARP=1` | dispositivo do host no adaptador por software (lento; não consegue reiniciar o driver) |
| `VICTORY_SHADER_DUMP=<dir>` | grava cada contêiner de shader e seu DXIL como `<hash>.bin` / `<hash>.dxil` quando é traduzido, para os hashes que as linhas do DRED e de timing citam (`dxc -dumpbin <hash>.dxil`) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | os loops dos shaders traduzidos param após `n` iterações por lane em vez de 2^20 (entradas próprias no cache de shaders; funciona também em builds de release); se um `n` menor deixa os quadros do mundo bem mais rápidos, são os loops que atingem o limite que os tornam lentos |
| `VICTORY_SHADER_SPIN_LIMIT=<n>` | loops que apenas relêem a memória até que outro workgroup ou fila a altere (spin waits) param após `n` iterações por lane em vez de 4096 (entradas próprias no cache de shaders); `1048576` restaura o limite geral para eles |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | grava o que o título reproduz (apenas o nosso processo) |
| `VICTORY_XMA_DUMP=<dir>` | salva o contexto e a entrada das primeiras 32 vozes XMA |

Todo build acrescenta exceções não tratadas, panics e falhas de GPU do host ao
`crashes.log` na pasta de dados do título.

Para um breakpoint de inicialização (`0x80000003`), execute o build de
diagnóstico com `VICTORY_CRASH_BREAKPOINTS=1`. Guarde a saída do console junto
com as novas entradas do `crashes.log`. Uma recusa de DLL pelo host inclui a
pilha do chamador na saída do console. A recusa é esperada para DLLs fora da
política do host, inclusive `rpcrt4.dll`.

No Series 1.0.2.1, a ausência do `appdata0.rpf` reproduziu o breakpoint em
`+0x2237178` antes da primeira janela. Restaure o arquivo do mesmo pacote ao
lado do executável do título. O `run --execute` verifica se consegue ler o
cabeçalho RPF9 antes de iniciar este build.

Para comparar a renderização por hardware e por software, repita com
`VICTORY_D3D12_WARP=1` e procure no console por
`host device on the software adapter (WARP)` para confirmar que o dispositivo
WARP foi criado. Compare o módulo e o offset da exceção, os frames da pilha e
as mensagens anteriores entre as duas execuções.

### Reportando uma falha de GPU

Uma remoção do dispositivo do host, um travamento ou um quadro que sai errado
é muito mais fácil de localizar com estas informações junto:

* o adaptador e o driver, como a janela do próprio título os enxerga (modelo
  da GPU, a versão do Adrenalin ou do GeForce e o número de build do Windows -
  uma atualização de driver entre dois relatos muda o que vale comparar);
* se `VICTORY_D3D12_WARP=1` reproduz o problema: uma falha que sobrevive ao
  adaptador por software é nossa, uma que não sobrevive aponta para o driver;
* as últimas linhas do `crashes.log`, que nomeiam o código que falhou e os
  comandos em andamento quando `VICTORY_D3D12_DRED=1` estava definido;
* as variáveis da tabela acima que estavam definidas na execução, e o perfil
  do build (`diag` ou release);
* quanto tempo levou, e se a tela foi reiniciada (um TDR) ou o processo parou.

Nada dos dados do título pertence ao relatório: sem dumps de shaders, sem
quadros, sem memória, conforme [docs/asset-policy.md](docs/asset-policy.md).
Nomes das nossas próprias funções, códigos de erro e tempos bastam para
começar.

[docs/debugging-notes-from-a-gta-v-port.md](docs/debugging-notes-from-a-gta-v-port.md)
reúne o que levou outro port do RAGE de "o mundo carrega" até "o jogo roda":
o que o título esperava e como cada espera foi encontrada.

## Licença

GPL-3.0-or-later. Veja [LICENSE](LICENSE).
