const std = @import("std");
const assert = @import("../quirks.zig").inlineAssert;
const Allocator = std.mem.Allocator;
const Action = @import("Binding.zig").Action;

/// A command is a named binding action that can be executed from
/// something like a command palette.
///
/// A command must be associated with a binding; all commands can be
/// mapped to traditional `keybind` configurations. This restriction
/// makes it so that there is nothing special about commands and likewise
/// it makes it trivial and consistent to define custom commands.
///
/// For apprt implementers: a command palette doesn't have to make use
/// of all the fields here. We try to provide as much information as
/// possible to make it easier to implement a command palette in the way
/// that makes the most sense for the application.
pub const Command = struct {
    action: Action,
    title: [:0]const u8,
    description: [:0]const u8 = "",

    /// ghostty_command_s
    pub const C = extern struct {
        action_key: [*:0]const u8,
        action: [*:0]const u8,
        title: [*:0]const u8,
        description: [*:0]const u8,
    };

    pub fn clone(self: *const Command, alloc: Allocator) Allocator.Error!Command {
        return .{
            .action = try self.action.clone(alloc),
            .title = try alloc.dupeZ(u8, self.title),
            .description = try alloc.dupeZ(u8, self.description),
        };
    }

    pub fn equal(self: Command, other: Command) bool {
        if (self.action.hash() != other.action.hash()) return false;
        if (!std.mem.eql(u8, self.title, other.title)) return false;
        if (!std.mem.eql(u8, self.description, other.description)) return false;
        return true;
    }

    /// Convert this command to a C struct at comptime.
    pub fn comptimeCval(self: Command) C {
        assert(@inComptime());

        return .{
            .action_key = @tagName(self.action),
            .action = std.fmt.comptimePrint("{f}", .{self.action}),
            .title = self.title,
            .description = self.description,
        };
    }

    /// Convert this command to a C struct at runtime.
    ///
    /// This shares memory with the original command.
    ///
    /// The action string is allocated using the provided allocator. You can
    /// free the slice directly if you need to but we recommend an arena
    /// for this.
    pub fn cval(self: Command, alloc: Allocator) Allocator.Error!C {
        var buf: std.Io.Writer.Allocating = .init(alloc);
        defer buf.deinit();
        self.action.format(&buf.writer) catch return error.OutOfMemory;
        const action = try buf.toOwnedSliceSentinel(0);

        return .{
            .action_key = @tagName(self.action),
            .action = action.ptr,
            .title = self.title,
            .description = self.description,
        };
    }

    /// Implements a comparison function for std.mem.sortUnstable
    /// and similar functions. The sorting is defined by Ghostty
    /// to be what we prefer. If a caller wants some other sorting,
    /// they should do it themselves.
    pub fn lessThan(_: void, lhs: Command, rhs: Command) bool {
        return std.ascii.orderIgnoreCase(lhs.title, rhs.title) == .lt;
    }
};

pub const defaults: []const Command = defaults: {
    @setEvalBranchQuota(100_000);

    var count: usize = 0;
    for (@typeInfo(Action.Key).@"enum".fields) |field| {
        const action = @field(Action.Key, field.name);
        count += actionCommands(action).len;
    }

    var result: [count]Command = undefined;
    var i: usize = 0;
    for (@typeInfo(Action.Key).@"enum".fields) |field| {
        const action = @field(Action.Key, field.name);
        const commands = actionCommands(action);
        for (commands) |cmd| {
            result[i] = cmd;
            i += 1;
        }
    }

    std.mem.sortUnstable(Command, &result, {}, Command.lessThan);

    assert(i == count);
    const final = result;
    break :defaults &final;
};

/// Defaults in C-compatible form.
pub const defaultsC: []const Command.C = defaults: {
    @setEvalBranchQuota(100_000);
    var result: [defaults.len]Command.C = undefined;
    for (defaults, 0..) |cmd, i| result[i] = cmd.comptimeCval();
    const final = result;
    break :defaults &final;
};

/// Returns the set of commands associated with this action key by
/// default. Not all actions should have commands. As a general guideline,
/// an action should have a command only if it is useful and reasonable
/// to appear in a command palette.
fn actionCommands(action: Action.Key) []const Command {
    // This is implemented as a function and switch rather than a
    // flat comptime const because we want to ensure we get a compiler
    // error when a new binding is added so that the contributor has
    // to consider whether that new binding should have commands or not.
    const result: []const Command = switch (action) {
        // Note: the use of `comptime` prefix on the return values
        // ensures that the data returned is all in the binary and
        // and not pointing to the stack.

        .reset => comptime &.{.{
            .action = .reset,
            .title = "Сбросить терминал",
            .description = "Вернуть терминал в чистое состояние.",
        }},

        .copy_to_clipboard => comptime &.{ .{
            .action = .{ .copy_to_clipboard = .mixed },
            .title = "Скопировать",
            .description = "Скопировать выделенное в буфер обмена — и простым текстом, и с оформлением.",
        }, .{
            .action = .{ .copy_to_clipboard = .plain },
            .title = "Скопировать выделенное как простой текст",
            .description = "Скопировать выделенное в буфер обмена без оформления.",
        }, .{
            .action = .{ .copy_to_clipboard = .vt },
            .title = "Скопировать выделенное с ANSI-кодами",
            .description = "Скопировать выделенное в буфер обмена вместе с управляющими ANSI-кодами.",
        }, .{
            .action = .{ .copy_to_clipboard = .html },
            .title = "Скопировать выделенное как HTML",
            .description = "Скопировать выделенное в буфер обмена в формате HTML.",
        } },

        .copy_url_to_clipboard => comptime &.{.{
            .action = .copy_url_to_clipboard,
            .title = "Скопировать ссылку",
            .description = "Скопировать ссылку под курсором в буфер обмена.",
        }},

        .copy_title_to_clipboard => comptime &.{.{
            .action = .copy_title_to_clipboard,
            .title = "Скопировать заголовок терминала",
            .description = "Скопировать заголовок терминала в буфер обмена, если он задан.",
        }},

        .paste_from_clipboard => comptime &.{.{
            .action = .paste_from_clipboard,
            .title = "Вставить",
            .description = "Вставить из буфера обмена.",
        }},

        .paste_from_selection => comptime &.{.{
            .action = .paste_from_selection,
            .title = "Вставить выделенное",
            .description = "Вставить текст из буфера выделения.",
        }},

        .start_search => comptime &.{.{
            .action = .start_search,
            .title = "Найти",
            .description = "Открыть поиск, если он ещё не открыт.",
        }},

        .search_selection => comptime &.{.{
            .action = .search_selection,
            .title = "Найти выделенное",
            .description = "Искать выделенный текст.",
        }},

        .end_search => comptime &.{.{
            .action = .end_search,
            .title = "Закрыть поиск",
            .description = "Закончить поиск и скрыть его панель.",
        }},

        .navigate_search => comptime &.{ .{
            .action = .{ .navigate_search = .next },
            .title = "Следующее совпадение",
            .description = "Перейти к следующему найденному.",
        }, .{
            .action = .{ .navigate_search = .previous },
            .title = "Предыдущее совпадение",
            .description = "Перейти к предыдущему найденному.",
        } },

        .increase_font_size => comptime &.{.{
            .action = .{ .increase_font_size = 1 },
            .title = "Увеличить шрифт",
            .description = "Увеличить шрифт на 1 пункт.",
        }},

        .decrease_font_size => comptime &.{.{
            .action = .{ .decrease_font_size = 1 },
            .title = "Уменьшить шрифт",
            .description = "Уменьшить шрифт на 1 пункт.",
        }},

        .reset_font_size => comptime &.{.{
            .action = .reset_font_size,
            .title = "Обычный размер шрифта",
            .description = "Вернуть размер шрифта из настроек.",
        }},

        .clear_screen => comptime &.{.{
            .action = .clear_screen,
            .title = "Очистить экран",
            .description = "Очистить экран и историю прокрутки.",
        }},

        .select_all => comptime &.{.{
            .action = .select_all,
            .title = "Выбрать все",
            .description = "Выделить весь текст на экране.",
        }},

        .scroll_to_top => comptime &.{.{
            .action = .scroll_to_top,
            .title = "В начало",
            .description = "Прокрутить в самое начало.",
        }},

        .scroll_to_bottom => comptime &.{.{
            .action = .scroll_to_bottom,
            .title = "В конец",
            .description = "Прокрутить в самый конец.",
        }},

        .scroll_to_selection => comptime &.{.{
            .action = .scroll_to_selection,
            .title = "К выделенному",
            .description = "Прокрутить к выделенному тексту.",
        }},

        .scroll_page_up => comptime &.{.{
            .action = .scroll_page_up,
            .title = "Страница вверх",
            .description = "Прокрутить на страницу вверх.",
        }},

        .scroll_page_down => comptime &.{.{
            .action = .scroll_page_down,
            .title = "Страница вниз",
            .description = "Прокрутить на страницу вниз.",
        }},

        .write_screen_file => comptime &.{
            .{
                .action = .{ .write_screen_file = .copy },
                .title = "Экран во временный файл — скопировать путь",
                .description = "Сохранить содержимое экрана во временный файл и скопировать путь к нему.",
            },
            .{
                .action = .{ .write_screen_file = .paste },
                .title = "Экран во временный файл — вставить путь",
                .description = "Сохранить содержимое экрана во временный файл и вставить путь к нему.",
            },
            .{
                .action = .{ .write_screen_file = .open },
                .title = "Экран во временный файл — открыть",
                .description = "Сохранить содержимое экрана во временный файл и открыть его.",
            },

            .{
                .action = .{ .write_screen_file = .{
                    .action = .copy,
                    .emit = .html,
                } },
                .title = "Экран в HTML-файл — скопировать путь",
                .description = "Сохранить экран как HTML во временный файл и скопировать путь к нему.",
            },
            .{
                .action = .{ .write_screen_file = .{
                    .action = .paste,
                    .emit = .html,
                } },
                .title = "Экран в HTML-файл — вставить путь",
                .description = "Сохранить экран как HTML во временный файл и вставить путь к нему.",
            },
            .{
                .action = .{ .write_screen_file = .{
                    .action = .open,
                    .emit = .html,
                } },
                .title = "Экран в HTML-файл — открыть",
                .description = "Сохранить экран как HTML во временный файл и открыть его.",
            },

            .{
                .action = .{ .write_screen_file = .{
                    .action = .copy,
                    .emit = .vt,
                } },
                .title = "Экран с ANSI-кодами в файл — скопировать путь",
                .description = "Сохранить экран с ANSI-кодами во временный файл и скопировать путь к нему.",
            },
            .{
                .action = .{ .write_screen_file = .{
                    .action = .paste,
                    .emit = .vt,
                } },
                .title = "Экран с ANSI-кодами в файл — вставить путь",
                .description = "Сохранить экран с ANSI-кодами во временный файл и вставить путь к нему.",
            },
            .{
                .action = .{ .write_screen_file = .{
                    .action = .open,
                    .emit = .vt,
                } },
                .title = "Экран с ANSI-кодами в файл — открыть",
                .description = "Сохранить экран с ANSI-кодами во временный файл и открыть его.",
            },
        },

        .write_selection_file => comptime &.{
            .{
                .action = .{ .write_selection_file = .copy },
                .title = "Выделенное во временный файл — скопировать путь",
                .description = "Сохранить выделенное во временный файл и скопировать путь к нему.",
            },
            .{
                .action = .{ .write_selection_file = .paste },
                .title = "Выделенное во временный файл — вставить путь",
                .description = "Сохранить выделенное во временный файл и вставить путь к нему.",
            },
            .{
                .action = .{ .write_selection_file = .open },
                .title = "Выделенное во временный файл — открыть",
                .description = "Сохранить выделенное во временный файл и открыть его.",
            },

            .{
                .action = .{ .write_selection_file = .{
                    .action = .copy,
                    .emit = .html,
                } },
                .title = "Выделенное в HTML-файл — скопировать путь",
                .description = "Сохранить выделенное как HTML во временный файл и скопировать путь к нему.",
            },
            .{
                .action = .{ .write_selection_file = .{
                    .action = .paste,
                    .emit = .html,
                } },
                .title = "Выделенное в HTML-файл — вставить путь",
                .description = "Сохранить выделенное как HTML во временный файл и вставить путь к нему.",
            },
            .{
                .action = .{ .write_selection_file = .{
                    .action = .open,
                    .emit = .html,
                } },
                .title = "Выделенное в HTML-файл — открыть",
                .description = "Сохранить выделенное как HTML во временный файл и открыть его.",
            },

            .{
                .action = .{ .write_selection_file = .{
                    .action = .copy,
                    .emit = .vt,
                } },
                .title = "Выделенное с ANSI-кодами в файл — скопировать путь",
                .description = "Сохранить выделенное с ANSI-кодами во временный файл и скопировать путь к нему.",
            },
            .{
                .action = .{ .write_selection_file = .{
                    .action = .paste,
                    .emit = .vt,
                } },
                .title = "Выделенное с ANSI-кодами в файл — вставить путь",
                .description = "Сохранить выделенное с ANSI-кодами во временный файл и вставить путь к нему.",
            },
            .{
                .action = .{ .write_selection_file = .{
                    .action = .open,
                    .emit = .vt,
                } },
                .title = "Выделенное с ANSI-кодами в файл — открыть",
                .description = "Сохранить выделенное с ANSI-кодами во временный файл и открыть его.",
            },
        },

        .new_window => comptime &.{.{
            .action = .new_window,
            .title = "Новое окно",
            .description = "Открыть новое окно.",
        }},

        .new_tab => comptime &.{.{
            .action = .new_tab,
            .title = "Новая вкладка",
            .description = "Открыть новую вкладку.",
        }},

        .move_tab => comptime &.{
            .{
                .action = .{ .move_tab = -1 },
                .title = "Вкладку влево",
                .description = "Передвинуть вкладку влево.",
            },
            .{
                .action = .{ .move_tab = 1 },
                .title = "Вкладку вправо",
                .description = "Передвинуть вкладку вправо.",
            },
        },

        .toggle_tab_overview => comptime &.{.{
            .action = .toggle_tab_overview,
            .title = "Обзор вкладок",
            .description = "Показать или скрыть обзор вкладок.",
        }},

        .prompt_surface_title => comptime &.{.{
            .action = .prompt_surface_title,
            .title = "Изменить заголовок терминала…",
            .description = "Задать новый заголовок этому терминалу.",
        }},

        .prompt_tab_title => comptime &.{.{
            .action = .prompt_tab_title,
            .title = "Изменить название вкладки…",
            .description = "Задать новое название этой вкладке.",
        }},

        .new_split => comptime &.{
            .{
                .action = .{ .new_split = .left },
                .title = "Разделить влево",
                .description = "Открыть новую панель слева.",
            },
            .{
                .action = .{ .new_split = .right },
                .title = "Разделить вправо",
                .description = "Открыть новую панель справа.",
            },
            .{
                .action = .{ .new_split = .up },
                .title = "Разделить вверх",
                .description = "Открыть новую панель сверху.",
            },
            .{
                .action = .{ .new_split = .down },
                .title = "Разделить вниз",
                .description = "Открыть новую панель снизу.",
            },
        },

        .goto_split => comptime &.{
            .{
                .action = .{ .goto_split = .previous },
                .title = "Панель: предыдущая",
                .description = "Перейти к предыдущей панели.",
            },
            .{
                .action = .{ .goto_split = .next },
                .title = "Панель: следующая",
                .description = "Перейти к следующей панели.",
            },
            .{
                .action = .{ .goto_split = .left },
                .title = "Панель: слева",
                .description = "Перейти к панели слева.",
            },
            .{
                .action = .{ .goto_split = .right },
                .title = "Панель: справа",
                .description = "Перейти к панели справа.",
            },
            .{
                .action = .{ .goto_split = .up },
                .title = "Панель: сверху",
                .description = "Перейти к панели сверху.",
            },
            .{
                .action = .{ .goto_split = .down },
                .title = "Панель: снизу",
                .description = "Перейти к панели снизу.",
            },
        },

        .goto_window => comptime &.{
            .{
                .action = .{ .goto_window = .previous },
                .title = "Окно: предыдущее",
                .description = "Перейти к предыдущему окну.",
            },
            .{
                .action = .{ .goto_window = .next },
                .title = "Окно: следующее",
                .description = "Перейти к следующему окну.",
            },
        },

        .toggle_split_zoom => comptime &.{.{
            .action = .toggle_split_zoom,
            .title = "Развернуть панель",
            .description = "Развернуть панель на всё окно или вернуть как было.",
        }},

        .toggle_readonly => comptime &.{.{
            .action = .toggle_readonly,
            .title = "Только чтение",
            .description = "Включить или выключить режим только для чтения.",
        }},

        .equalize_splits => comptime &.{.{
            .action = .equalize_splits,
            .title = "Выровнять панели",
            .description = "Сделать все панели одного размера.",
        }},

        .reset_window_size => comptime &.{.{
            .action = .reset_window_size,
            .title = "Исходный размер окна",
            .description = "Вернуть окну размер по умолчанию.",
        }},

        .inspector => comptime &.{.{
            .action = .{ .inspector = .toggle },
            .title = "Инспектор терминала",
            .description = "Показать или скрыть инспектор терминала.",
        }},

        .show_gtk_inspector => comptime &.{.{
            .action = .show_gtk_inspector,
            .title = "Инспектор GTK",
            .description = "Показать инспектор GTK.",
        }},

        .show_on_screen_keyboard => comptime &.{.{
            .action = .show_on_screen_keyboard,
            .title = "Экранная клавиатура",
            .description = "Показать экранную клавиатуру, если она есть.",
        }},

        .open_config => comptime &.{.{
            .action = .open_config,
            .title = "Настройки",
            .description = "Открыть окно настроек.",
        }},

        .reload_config => comptime &.{.{
            .action = .reload_config,
            .title = "Перечитать настройки",
            .description = "Заново прочитать файл настроек.",
        }},

        .close_surface => comptime &.{.{
            .action = .close_surface,
            .title = "Закрыть терминал",
            .description = "Закрыть этот терминал.",
        }},

        .close_tab => comptime &.{
            .{
                .action = .{ .close_tab = .this },
                .title = "Закрыть вкладку",
                .description = "Закрыть эту вкладку.",
            },
            .{
                .action = .{ .close_tab = .other },
                .title = "Закрыть другие вкладки",
                .description = "Закрыть в этом окне все вкладки, кроме текущей.",
            },
            .{
                .action = .{ .close_tab = .right },
                .title = "Закрыть вкладки справа",
                .description = "Закрыть все вкладки правее текущей.",
            },
        },

        .close_window => comptime &.{.{
            .action = .close_window,
            .title = "Закрыть окно",
            .description = "Закрыть это окно.",
        }},

        .close_all_windows => comptime &.{.{
            .action = .close_all_windows,
            .title = "Закрыть все окна",
            .description = "Закрыть все окна.",
        }},

        .toggle_maximize => comptime &.{.{
            .action = .toggle_maximize,
            .title = "Развернуть окно",
            .description = "Развернуть окно или вернуть прежний размер.",
        }},

        .toggle_fullscreen => comptime &.{.{
            .action = .toggle_fullscreen,
            .title = "Полноэкранный режим",
            .description = "Включить или выключить полноэкранный режим.",
        }},

        .toggle_window_decorations => comptime &.{.{
            .action = .toggle_window_decorations,
            .title = "Рамка окна",
            .description = "Показать или скрыть рамку окна.",
        }},

        .toggle_window_float_on_top => comptime &.{.{
            .action = .toggle_window_float_on_top,
            .title = "Поверх всех окон",
            .description = "Держать окно поверх остальных или нет.",
        }},

        .toggle_secure_input => comptime &.{.{
            .action = .toggle_secure_input,
            .title = "Защищённый ввод",
            .description = "Включить или выключить защищённый ввод с клавиатуры.",
        }},

        .toggle_mouse_reporting => comptime &.{.{
            .action = .toggle_mouse_reporting,
            .title = "Мышь в программы",
            .description = "Передавать ли события мыши программам в терминале.",
        }},

        .toggle_background_opacity => comptime &.{.{
            .action = .toggle_background_opacity,
            .title = "Прозрачность фона",
            .description = "Включить или выключить прозрачность окна, если она задана.",
        }},

        // VibeTerminal: обновления через Sparkle выключены, команда не нужна.
        .check_for_updates => comptime &.{},

        .undo => comptime &.{.{
            .action = .undo,
            .title = "Отменить",
            .description = "Отменить последнее действие.",
        }},

        .redo => comptime &.{.{
            .action = .redo,
            .title = "Повторить",
            .description = "Повторить отменённое действие.",
        }},

        .quit => comptime &.{.{
            .action = .quit,
            .title = "Завершить",
            .description = "Завершить приложение.",
        }},

        .text => comptime &.{.{
            .action = .{ .text = "👻" },
            .title = "Ghostty",
            .description = "Немного Ghostty в твоём терминале.",
        }},

        // No commands because they're parameterized and there
        // aren't obvious values users would use. It is possible that
        // these may have commands in the future if there are very
        // common values that users tend to use.
        .csi,
        .esc,
        .cursor_key,
        .set_font_size,
        .set_surface_title,
        .set_tab_title,
        .search,
        .scroll_to_row,
        .scroll_page_fractional,
        .scroll_page_lines,
        .adjust_selection,
        .jump_to_prompt,
        .write_scrollback_file,
        .goto_tab,
        .resize_split,
        .activate_key_table,
        .activate_key_table_once,
        .deactivate_key_table,
        .deactivate_all_key_tables,
        .end_key_sequence,
        .crash,
        => comptime &.{},

        // No commands because I'm not sure they make sense in a command
        // palette context.
        .toggle_command_palette,
        .toggle_quick_terminal,
        .toggle_visibility,
        .previous_tab,
        .next_tab,
        .last_tab,
        => comptime &.{},

        // No commands for obvious reasons
        .ignore,
        .unbind,
        => comptime &.{},
    };

    // All generated commands should have the same action as the
    // action passed in.
    for (result) |cmd| assert(cmd.action == action);

    return result;
}

test "command defaults" {
    // This just ensures that defaults is analyzed and works.
    const testing = std.testing;
    try testing.expect(defaults.len > 0);
    try testing.expectEqual(defaults.len, defaultsC.len);
}
