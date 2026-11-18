# Chat rename and project of a chat: mockups

Task 496. **Decided (owner, message 5242): none of A, B, C; a chat's Home row has three lines: its name, the projects it runs in, its message.** Built as in docs/web-client-tasks.md "Home" and docs/tasks.md "Chats". The options below are kept as the record.

The mockups: The owner asked (messages 5143, 5164, 5166) to rename a chat and to see which project a chat belongs to, without the `project ·` prefix that takes message space on a Home row. Nothing here is decided; the owner picks.

Method: static pages made by [chat-rename/build.py](chat-rename/build.py), with the real `app.css`, the default theme's tokens (`theme.css`, written from `theme.toml`) and the rules a build would add ([mock.css](chat-rename/mock.css)). Captures at 390 px, scale 2, light and dark, by headless Chrome. The data is made up in the shape of real chats: project `studio-app` with chat `web-income` (shown as "Fee table" once renamed), and project `wsx`. Design lint on all 18 pages: 0 errors; the warns are the same ones the unchanged row has.

## Today

Line 2 starts with the project (`studio-app · Draft ready…`), so the message loses space. The header shows only the chat id. There is no way to rename.

[Home](chat-rename/before-home-light-390.png) · [Home dark](chat-rename/before-home-dark-390.png) · [Header](chat-rename/before-chat-light-390.png) · [Header dark](chat-rename/before-chat-dark-390.png)

## Rename (the same in the three options)

A chat keeps its id (folder, links, `say --to`). Rename sets a name shown instead of it. The chat's ⋯ menu gets "Rename chat" before "Close chat". It opens a sheet like New chat with one field: [sheet](chat-rename/rename-sheet-light-390.png) · [dark](chat-rename/rename-sheet-dark-390.png). Open question: a long press on a Home row as a second way in. A press and hold is free in A and B; the row already has a tap, a swipe left (Dismiss) and a swipe right (Reply), and task 499 is changing those swipes.

## Option A: the project as a small tag after the name

Line 1 is the name, then the project in muted text. Line 2 is only the message. A chat whose shown name is its project (the project's first chat) shows no tag. In the header the project is the small second line under the name. A long name cuts the tag first.

[Home](chat-rename/a-home-light-390.png) · [dark](chat-rename/a-home-dark-390.png) · [Header with menu](chat-rename/a-chat-light-390.png) · [dark](chat-rename/a-chat-dark-390.png)

## Option B: a project's chats under its own row

Home is ordered by project. The project's first chat is its row; the other chats of that project sit under it, one step in. No row repeats the project. In the header the project comes first, then the name: `studio-app › Fee table`. Cost: Home no longer lists chats newest first across projects.

[Home](chat-rename/b-home-light-390.png) · [dark](chat-rename/b-home-dark-390.png) · [Header with menu](chat-rename/b-chat-light-390.png) · [dark](chat-rename/b-chat-dark-390.png)

## Option C: a two-letter mark of the project at the row start

Each row starts with the first two letters of its project in a round mark. Lines 1 and 2 are untouched. The header shows the same mark, with the project name as the small second line. Cost: 44 px of row width, and two projects that start with the same letters look alike.

[Home](chat-rename/c-home-light-390.png) · [dark](chat-rename/c-home-dark-390.png) · [Header with menu](chat-rename/c-chat-light-390.png) · [dark](chat-rename/c-chat-dark-390.png)

## Not mocked

Wide screens (1280 px) and the TUI. The pages have no bottom tab bar. The long-press on a row is described, not drawn. Order of the remaining work after the owner's choice: the shown-name reader and the 40 character rule in `wsx-core` `task/chats.rs`, `wsx task chat rename`, the web route, the web and TUI views, `docs/tasks.md` "Chats" and `docs/web-client-tasks.md` "Home".
