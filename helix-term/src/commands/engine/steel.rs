use arc_swap::ArcSwapAny;
use crossterm::event::{Event, KeyCode, KeyModifiers};
use helix_core::{
    diagnostic::Severity,
    extensions::steel_implementations::{rope_module, SteelRopeSlice},
    find_workspace, graphemes,
    shellwords::Shellwords,
    syntax::{AutoPairConfig, SoftWrap},
    Range, Selection, Tendril,
};
use helix_event::register_hook;
use helix_view::{
    annotations::diagnostics::DiagnosticFilter,
    document::Mode,
    editor::{
        Action, AutoSave, BufferLine, ConfigEvent, CursorShapeConfig, FilePickerConfig,
        GutterConfig, IndentGuidesConfig, LineEndingConfig, LineNumber, LspConfig, SearchConfig,
        SmartTabConfig, StatusLineConfig, TerminalConfig, WhitespaceConfig,
    },
    extension::document_id_to_usize,
    input::KeyEvent,
    theme::Color,
    DocumentId, Editor, Theme, ViewId,
};
use once_cell::sync::{Lazy, OnceCell};
use steel::{
    gc::{unsafe_erased_pointers::CustomReference, ShareableMut},
    rvals::{as_underlying_type, IntoSteelVal, SteelString},
    steel_vm::{
        engine::Engine, mutex_lock, mutex_unlock, register_fn::RegisterFn, ThreadStateController,
    },
    steelerr, SteelErr, SteelVal,
};

use std::sync::Arc;
use std::{
    borrow::Cow,
    collections::HashMap,
    error::Error,
    path::PathBuf,
    sync::{atomic::AtomicBool, Mutex, MutexGuard},
    time::Duration,
};

use steel::{rvals::Custom, steel_vm::builtin::BuiltInModule};

use crate::{
    commands::insert,
    compositor::{self, Component, Compositor},
    config::Config,
    events::{OnModeSwitch, PostCommand, PostInsertChar},
    job::{self, Callback},
    keymap::{self, merge_keys, KeyTrie, KeymapResult},
    ui::{self, picker::PathOrId, PickerColumn, Popup, Prompt, PromptEvent},
};

use components::SteelDynamicComponent;

use super::{
    components::{self, helix_component_module},
    Context, MappableCommand, TYPABLE_COMMAND_LIST,
};
use insert::{insert_char, insert_string};

pub static INTERRUPT_HANDLER: OnceCell<InterruptHandler> = OnceCell::new();

// TODO: Use this for the available commands.
// We just have to look at functions that have been defined at
// the top level, _after_ they
pub static GLOBAL_OFFSET: OnceCell<usize> = OnceCell::new();
// pub static AVAILABLE_FUNCTIONS: Lazy<RwLock<Vec<String>>> = Lazy::new(|| RwLock::new(Vec::new()));

// The Steel scripting engine instance. This is what drives the whole integration.
pub static GLOBAL_ENGINE: Lazy<Mutex<steel::steel_vm::engine::Engine>> = Lazy::new(|| {
    let engine = steel::steel_vm::engine::Engine::new();

    // Any function after this point can be used for looking at "new" functions
    GLOBAL_OFFSET.set(engine.readable_globals(0).len()).unwrap();

    let controller = engine.get_thread_state_controller();
    let running = Arc::new(AtomicBool::new(false));

    fn is_event_available() -> std::io::Result<bool> {
        crossterm::event::poll(Duration::from_millis(10))
    }

    let controller_clone = controller.clone();
    let running_clone = running.clone();

    // TODO: Only allow interrupt after a certain amount of time...
    // perhaps something like, 500 ms? That way interleaving calls to
    // steel functions don't accidentally cause an interrupt.
    let thread_handle = std::thread::spawn(move || {
        let controller = controller_clone;
        let running = running_clone;

        loop {
            std::thread::park();

            while running.load(std::sync::atomic::Ordering::Relaxed) {
                if is_event_available().unwrap_or(false) {
                    let event = crossterm::event::read();

                    if let Ok(Event::Key(crossterm::event::KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers: KeyModifiers::CONTROL,
                        ..
                    })) = event
                    {
                        controller.interrupt();
                        break;
                    }
                }
            }
        }
    });

    INTERRUPT_HANDLER
        .set(InterruptHandler {
            controller: controller.clone(),
            running: running.clone(),
            handle: thread_handle,
        })
        .ok();

    Mutex::new(configure_engine_impl(engine))
});

fn acquire_engine_lock() -> MutexGuard<'static, Engine> {
    GLOBAL_ENGINE.lock().unwrap()
}

/// Run a function with exclusive access to the engine. This only
/// locks the engine that is running on the main thread.
pub fn enter_engine<F, R>(f: F) -> R
where
    F: FnOnce(&mut Engine) -> R,
{
    (f)(&mut acquire_engine_lock())
}

pub struct InterruptHandler {
    controller: ThreadStateController,
    running: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

pub fn with_interrupt_handler<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let handler = INTERRUPT_HANDLER.get().unwrap();
    handler.handle.thread().unpark();

    handler
        .running
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let res = (f)();

    handler.controller.resume();
    handler
        .running
        .store(false, std::sync::atomic::Ordering::Relaxed);

    res
}

pub struct KeyMapApi {
    default_keymap: fn() -> EmbeddedKeyMap,
    empty_keymap: fn() -> EmbeddedKeyMap,
    string_to_embedded_keymap: fn(String) -> EmbeddedKeyMap,
    merge_keybindings: fn(&mut EmbeddedKeyMap, EmbeddedKeyMap),
    is_keymap: fn(SteelVal) -> bool,
    deep_copy_keymap: fn(EmbeddedKeyMap) -> EmbeddedKeyMap,
}

impl KeyMapApi {
    fn new() -> Self {
        KeyMapApi {
            default_keymap,
            empty_keymap,
            string_to_embedded_keymap,
            merge_keybindings,
            is_keymap,
            deep_copy_keymap,
        }
    }
}

// Handle buffer and extension specific keybindings in userspace.
pub static BUFFER_OR_EXTENSION_KEYBINDING_MAP: Lazy<SteelVal> =
    Lazy::new(|| SteelVal::boxed(SteelVal::empty_hashmap()));

pub static REVERSE_BUFFER_MAP: Lazy<SteelVal> =
    Lazy::new(|| SteelVal::boxed(SteelVal::empty_hashmap()));

fn load_component_api(engine: &mut Engine, generate_sources: bool) {
    let module = helix_component_module();

    if generate_sources {
        configure_lsp_builtins("component", &module);
    }

    engine.register_module(module);
}

fn load_keymap_api(engine: &mut Engine, api: KeyMapApi, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/keymaps");

    module.register_fn("helix-empty-keymap", api.empty_keymap);
    module.register_fn("helix-default-keymap", api.default_keymap);
    module.register_fn("helix-merge-keybindings", api.merge_keybindings);
    module.register_fn("helix-string->keymap", api.string_to_embedded_keymap);
    module.register_fn("keymap?", api.is_keymap);

    module.register_fn("helix-deep-copy-keymap", api.deep_copy_keymap);

    // This should be associated with a corresponding scheme module to wrap this up
    module.register_value(
        "*buffer-or-extension-keybindings*",
        BUFFER_OR_EXTENSION_KEYBINDING_MAP.clone(),
    );
    module.register_value("*reverse-buffer-map*", REVERSE_BUFFER_MAP.clone());
    module.register_fn("keymap-update-documentation!", update_documentation);

    if generate_sources {
        configure_lsp_builtins("keymap", &module)
    }

    engine.register_module(module);
}

fn load_static_commands(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/static");

    let mut builtin_static_command_module = if generate_sources {
        "(require-builtin helix/core/static as helix.static.)".to_string()
    } else {
        "".to_string()
    };

    for command in TYPABLE_COMMAND_LIST {
        let func = |cx: &mut Context| {
            let mut cx = compositor::Context {
                editor: cx.editor,
                scroll: None,
                jobs: cx.jobs,
            };

            (command.fun)(&mut cx, &[], PromptEvent::Validate)
        };

        module.register_fn(command.name, func);
    }

    // Register everything in the static command list as well
    // These just accept the context, no arguments
    for command in MappableCommand::STATIC_COMMAND_LIST {
        if let MappableCommand::Static { name, fun, doc } = command {
            module.register_fn(name, fun);

            if generate_sources {
                let mut docstring = doc
                    .lines()
                    .map(|x| {
                        let mut line = ";;".to_string();
                        line.push_str(x);
                        line.push_str("\n");
                        line
                    })
                    .collect::<String>();

                docstring.pop();

                builtin_static_command_module.push_str(&format!(
                    r#"
(provide {})
;;@doc
{}
(define ({})
    (helix.static.{} *helix.cx*))
"#,
                    name, docstring, name, name
                ));
            }
        }
    }

    let mut template_function_arity_1 = |name: &str, doc: &str| {
        if generate_sources {
            let mut docstring = doc
                .lines()
                .map(|x| {
                    let mut line = ";;".to_string();
                    line.push_str(x);
                    line.push_str("\n");
                    line
                })
                .collect::<String>();

            docstring.pop();

            builtin_static_command_module.push_str(&format!(
                r#"
(provide {})
;;@doc
{}
(define ({} arg)
    (helix.static.{} *helix.cx* arg))
"#,
                name, docstring, name, name
            ));
        }
    };

    macro_rules! function1 {
        ($name:expr, $function:expr, $doc:expr) => {{
            module.register_fn($name, $function);
            template_function_arity_1($name, $doc);
        }};
    }

    // Adhoc static commands that probably needs evaluating
    // Arity 1
    function1!(
        "insert_char",
        insert_char,
        "Insert a given character at the cursor cursor position"
    );
    function1!(
        "insert_string",
        insert_string,
        "Insert a given string at the current cursor position"
    );

    function1!(
        "set-current-selection-object!",
        set_selection,
        "Update the selection object to the current selection within the editor"
    );

    function1!(
        "regex-selection",
        regex_selection,
        "Run the given regex within the existing buffer"
    );

    function1!(
        "replace-selection-with",
        replace_selection,
        "Replace the existing selection with the given string"
    );

    function1!(
        "cx->current-file",
        current_path,
        "Get the currently focused file path"
    );

    function1!(
        "enqueue-expression-in-engine",
        run_expression_in_engine,
        "Enqueue an expression to run at the top level context, 
        after the existing function context has exited."
    );

    let mut template_function_arity_0 = |name: &str| {
        if generate_sources {
            builtin_static_command_module.push_str(&format!(
                r#"
(provide {})
(define ({})
    (helix.static.{} *helix.cx*))
"#,
                name, name, name
            ));
        }
    };

    macro_rules! function0 {
        ($name:expr, $function:expr) => {{
            module.register_fn($name, $function);
            template_function_arity_0($name);
        }};
    }

    function0!("current_selection", get_selection);
    function0!("load-buffer!", load_buffer);
    function0!("current-highlighted-text!", get_highlighted_text);
    function0!("get-current-line-number", current_line_number);
    function0!("current-selection-object", current_selection);
    function0!("get-helix-cwd", get_helix_cwd);
    function0!("move-window-far-left", move_window_to_the_left);
    function0!("move-window-far-right", move_window_to_the_right);

    let mut template_function_no_context = |name: &str| {
        if generate_sources {
            builtin_static_command_module.push_str(&format!(
                r#"
(provide {})
(define {} helix.static.{})                
            "#,
                name, name, name
            ))
        }
    };

    module.register_fn("get-helix-scm-path", get_helix_scm_path);
    module.register_fn("get-init-scm-path", get_init_scm_path);

    template_function_no_context("get-helix-scm-path");
    template_function_no_context("get-init-scm-path");

    if generate_sources {
        let mut target_directory = helix_runtime_search_path();

        if !target_directory.exists() {
            std::fs::create_dir(&target_directory).unwrap();
        }

        target_directory.push("static.scm");

        std::fs::write(target_directory, builtin_static_command_module).unwrap();
    }

    if generate_sources {
        configure_lsp_builtins("static", &module);
    }

    engine.register_module(module);
}

fn load_typed_commands(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/typable".to_string());

    let mut builtin_typable_command_module = if generate_sources {
        "(require-builtin helix/core/typable as helix.)".to_string()
    } else {
        "".to_string()
    };

    // Register everything in the typable command list. Now these are all available
    for command in TYPABLE_COMMAND_LIST {
        let func = |cx: &mut Context, args: &[Cow<str>]| {
            let mut cx = compositor::Context {
                editor: cx.editor,
                scroll: None,
                jobs: cx.jobs,
            };

            (command.fun)(&mut cx, args, PromptEvent::Validate)
        };

        module.register_fn(command.name, func);

        if generate_sources {
            // Create an ephemeral builtin module to reference until I figure out how
            // to wrap the functions with a reference to the engine context better.
            builtin_typable_command_module.push_str(&format!(
                r#"
(provide {})

;;@doc
{}
(define ({} . args)
    (helix.{} *helix.cx* args))
"#,
                command.name,
                {
                    // Ugly hack to drop the extra newline from
                    // the docstring
                    let mut docstring = command
                        .doc
                        .lines()
                        .map(|x| {
                            let mut line = ";;".to_string();
                            line.push_str(x);
                            line.push_str("\n");
                            line
                        })
                        .collect::<String>();

                    docstring.pop();

                    docstring
                },
                command.name,
                command.name
            ));
        }
    }

    if generate_sources {
        let mut target_directory = helix_runtime_search_path();
        if !target_directory.exists() {
            std::fs::create_dir(&target_directory).unwrap();
        }

        target_directory.push("commands.scm");

        std::fs::write(target_directory, builtin_typable_command_module).unwrap();
    }

    if generate_sources {
        configure_lsp_builtins("typed", &module);
    }

    engine.register_module(module);
}

fn get_option_value(cx: &mut Context, option: String) -> anyhow::Result<SteelVal> {
    let key_error = || anyhow::anyhow!("Unknown key `{}`", option);

    let config = serde_json::json!(std::ops::Deref::deref(&cx.editor.config()));
    let pointer = format!("/{}", option.replace('.', "/"));
    let value = config.pointer(&pointer).ok_or_else(key_error)?;
    Ok(value.to_owned().into_steelval().unwrap())
}

// File picker configurations
fn fp_hidden(config: &mut FilePickerConfig, option: bool) {
    config.hidden = option;
}

fn fp_follow_symlinks(config: &mut FilePickerConfig, option: bool) {
    config.follow_symlinks = option;
}

fn fp_deduplicate_links(config: &mut FilePickerConfig, option: bool) {
    config.deduplicate_links = option;
}

fn fp_parents(config: &mut FilePickerConfig, option: bool) {
    config.parents = option;
}

fn fp_ignore(config: &mut FilePickerConfig, option: bool) {
    config.ignore = option;
}

fn fp_git_ignore(config: &mut FilePickerConfig, option: bool) {
    config.git_ignore = option;
}

fn fp_git_global(config: &mut FilePickerConfig, option: bool) {
    config.git_global = option;
}

fn fp_git_exclude(config: &mut FilePickerConfig, option: bool) {
    config.git_exclude = option;
}

fn fp_max_depth(config: &mut FilePickerConfig, option: Option<usize>) {
    config.max_depth = option;
}

// Soft wrap configurations
fn sw_enable(config: &mut SoftWrap, option: Option<bool>) {
    config.enable = option;
}

fn sw_max_wrap(config: &mut SoftWrap, option: Option<u16>) {
    config.max_wrap = option;
}

fn sw_max_indent_retain(config: &mut SoftWrap, option: Option<u16>) {
    config.max_indent_retain = option;
}

fn sw_wrap_indicator(config: &mut SoftWrap, option: Option<String>) {
    config.wrap_indicator = option;
}

fn wrap_at_text_width(config: &mut SoftWrap, option: Option<bool>) {
    config.wrap_at_text_width = option;
}

fn load_configuration_api(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/configuration");

    module.register_fn("update-configuration!", |ctx: &mut Context| {
        ctx.editor
            .config_events
            .0
            .send(ConfigEvent::Change)
            .unwrap();
    });

    module.register_fn("get-config-option-value", get_option_value);

    module
        .register_fn("raw-file-picker", || FilePickerConfig::default())
        .register_fn("register-file-picker", HelixConfiguration::file_picker)
        .register_fn("fp-hidden", fp_hidden)
        .register_fn("fp-follow-symlinks", fp_follow_symlinks)
        .register_fn("fp-deduplicate-links", fp_deduplicate_links)
        .register_fn("fp-parents", fp_parents)
        .register_fn("fp-ignore", fp_ignore)
        .register_fn("fp-git-ignore", fp_git_ignore)
        .register_fn("fp-git-global", fp_git_global)
        .register_fn("fp-git-exclude", fp_git_exclude)
        .register_fn("fp-max-depth", fp_max_depth);

    module
        .register_fn("raw-soft-wrap", || SoftWrap::default())
        .register_fn("register-soft-wrap", HelixConfiguration::soft_wrap)
        .register_fn("sw-enable", sw_enable)
        .register_fn("sw-max-wrap", sw_max_wrap)
        .register_fn("sw-max-indent-retain", sw_max_indent_retain)
        .register_fn("sw-wrap-indicator", sw_wrap_indicator)
        .register_fn("sw-wrap-at-text-width", wrap_at_text_width);

    module
        .register_fn("scrolloff", HelixConfiguration::scrolloff)
        .register_fn("scroll_lines", HelixConfiguration::scroll_lines)
        .register_fn("mouse", HelixConfiguration::mouse)
        .register_fn("shell", HelixConfiguration::shell)
        .register_fn("line-number", HelixConfiguration::line_number)
        .register_fn("cursorline", HelixConfiguration::cursorline)
        .register_fn("cursorcolumn", HelixConfiguration::cursorcolumn)
        .register_fn("middle-click-paste", HelixConfiguration::middle_click_paste)
        .register_fn("auto-pairs", HelixConfiguration::auto_pairs)
        // Specific constructors for the auto pairs configuration
        .register_fn("auto-pairs-default", |enabled: bool| {
            AutoPairConfig::Enable(enabled)
        })
        .register_fn("auto-pairs-map", |map: HashMap<char, char>| {
            AutoPairConfig::Pairs(map)
        })
        // TODO: Finish this up
        .register_fn("auto-save-default", || AutoSave::default())
        .register_fn(
            "auto-save-after-delay-enable",
            HelixConfiguration::auto_save_after_delay_enable,
        )
        .register_fn(
            "inline-diagnostics-cursor-line-enable",
            HelixConfiguration::inline_diagnostics_cursor_line_enable,
        )
        .register_fn(
            "inline-diagnostics-end-of-line-enable",
            HelixConfiguration::inline_diagnostics_end_of_line_enable,
        )
        .register_fn("auto-completion", HelixConfiguration::auto_completion)
        .register_fn("auto-format", HelixConfiguration::auto_format)
        .register_fn("auto-save", HelixConfiguration::auto_save)
        .register_fn("text-width", HelixConfiguration::text_width)
        .register_fn("idle-timeout", HelixConfiguration::idle_timeout)
        .register_fn("completion-timeout", HelixConfiguration::completion_timeout)
        .register_fn(
            "preview-completion-insert",
            HelixConfiguration::preview_completion_insert,
        )
        .register_fn(
            "completion-trigger-len",
            HelixConfiguration::completion_trigger_len,
        )
        .register_fn("completion-replace", HelixConfiguration::completion_replace)
        .register_fn("auto-info", HelixConfiguration::auto_info)
        .register_fn("cursor-shape", HelixConfiguration::cursor_shape)
        .register_fn("true-color", HelixConfiguration::true_color)
        .register_fn(
            "insert-final-newline",
            HelixConfiguration::insert_final_newline,
        )
        .register_fn("color-modes", HelixConfiguration::color_modes)
        .register_fn("gutters", HelixConfiguration::gutters)
        // .register_fn("file-picker", HelixConfiguration::file_picker)
        .register_fn("statusline", HelixConfiguration::statusline)
        .register_fn("undercurl", HelixConfiguration::undercurl)
        .register_fn("search", HelixConfiguration::search)
        .register_fn("lsp", HelixConfiguration::lsp)
        .register_fn("terminal", HelixConfiguration::terminal)
        .register_fn("rulers", HelixConfiguration::rulers)
        .register_fn("whitespace", HelixConfiguration::whitespace)
        .register_fn("bufferline", HelixConfiguration::bufferline)
        .register_fn("indent-guides", HelixConfiguration::indent_guides)
        .register_fn("soft-wrap", HelixConfiguration::soft_wrap)
        .register_fn(
            "workspace-lsp-roots",
            HelixConfiguration::workspace_lsp_roots,
        )
        .register_fn(
            "default-line-ending",
            HelixConfiguration::default_line_ending,
        )
        .register_fn("smart-tab", HelixConfiguration::smart_tab);

    // Keybinding stuff
    module
        .register_fn("keybindings", HelixConfiguration::keybindings)
        .register_fn("get-keybindings", HelixConfiguration::get_keybindings);

    if generate_sources {
        let mut builtin_configuration_module =
            "(require-builtin helix/core/configuration as helix.)".to_string();

        builtin_configuration_module.push_str(&format!(
            r#"
(provide update-configuration!)
(define (update-configuration!)
    (helix.update-configuration! *helix.config*))
"#,
        ));

        builtin_configuration_module.push_str(&format!(
            r#"
(provide get-config-option-value)
(define (get-config-option-value arg)
    (helix.get-config-option-value *helix.cx* arg))
"#,
        ));

        // Register the get keybindings function
        builtin_configuration_module.push_str(&format!(
            r#"
(provide get-keybindings)
(define (get-keybindings)
    (helix.get-keybindings *helix.config*))
"#,
        ));

        let mut template_soft_wrap = |name: &str| {
            builtin_configuration_module.push_str(&format!(
                r#"
(provide {})
(define ({} arg)
    (lambda (picker) 
            (helix.{} picker arg)
            picker))
"#,
                name, name, name
            ));
        };

        let soft_wrap_functions = &[
            "sw-enable",
            "sw-max-wrap",
            "sw-max-indent-retain",
            "sw-wrap-indicator",
            "sw-wrap-at-text-width",
        ];

        for name in soft_wrap_functions {
            template_soft_wrap(name);
        }

        let mut template_file_picker_function = |name: &str| {
            builtin_configuration_module.push_str(&format!(
                r#"
(provide {})
(define ({} arg)
    (lambda (picker) 
            (helix.{} picker arg)
            picker))
"#,
                name, name, name
            ));
        };

        let file_picker_functions = &[
            "fp-hidden",
            "fp-follow-symlinks",
            "fp-deduplicate-links",
            "fp-parents",
            "fp-ignore",
            "fp-git-ignore",
            "fp-git-global",
            "fp-git-exclude",
            "fp-max-depth",
        ];

        for name in file_picker_functions {
            template_file_picker_function(name);
        }

        builtin_configuration_module.push_str(&format!(
            r#"
(provide file-picker)
(define (file-picker . args)
    (helix.register-file-picker
        *helix.config*
        (foldl (lambda (func config) (func config)) (helix.raw-file-picker) args)))
"#,
        ));

        builtin_configuration_module.push_str(&format!(
            r#"
(provide soft-wrap)
(define (soft-wrap . args)
    (helix.register-soft-wrap
        *helix.config*
        (foldl (lambda (func config) (func config)) (helix.raw-soft-wrap) args)))
"#,
        ));

        let mut template_function_arity_1 = |name: &str| {
            builtin_configuration_module.push_str(&format!(
                r#"
(provide {})
(define ({} arg)
    (helix.{} *helix.config* arg))
"#,
                name, name, name
            ));
        };

        let functions = &[
            "scrolloff",
            "scroll_lines",
            "mouse",
            "shell",
            "line-number",
            "cursorline",
            "cursorcolumn",
            "middle-click-paste",
            "auto-pairs",
            "auto-completion",
            "auto-format",
            "auto-save",
            "text-width",
            "idle-timeout",
            "completion-timeout",
            "preview-completion-insert",
            "completion-trigger-len",
            "completion-replace",
            "auto-info",
            "cursor-shape",
            "true-color",
            "insert-final-newline",
            "color-modes",
            "gutters",
            "statusline",
            "undercurl",
            "search",
            "lsp",
            "terminal",
            "rulers",
            "whitespace",
            "bufferline",
            "indent-guides",
            "workspace-lsp-roots",
            "default-line-ending",
            "smart-tab",
            "keybindings",
            "inline-diagnostics-cursor-line-enable",
            "inline-diagnostics-end-of-line-enable",
        ];

        for func in functions {
            template_function_arity_1(func);
        }

        let mut target_directory = helix_runtime_search_path();

        if !target_directory.exists() {
            std::fs::create_dir(&target_directory).unwrap();
        }

        target_directory.push("configuration.scm");

        std::fs::write(target_directory, builtin_configuration_module).unwrap();
    }

    if generate_sources {
        configure_lsp_builtins("configuration", &module);
    }

    engine.register_module(module);
}

fn languages_api(engine: &mut Engine, generate_sources: bool) {
    // TODO: Just look at the `cx.editor.syn_loader` for how to
    // manipulate the languages bindings
    todo!()
}

// fn test(ctx: &mut Context) {
//     ctx.editor.syn_loader.load()
// }

// TODO:
// This isn't the best API since it pretty much requires deserializing
// the whole theme model each time. While its not _horrible_, it is
// certainly not as efficient as it could be. If we could just edit
// the loaded theme in memory already, then it would be a bit nicer.
fn load_theme_api(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/themes");
    module
        .register_fn("hashmap->theme", theme_from_json_string)
        .register_fn("add-theme!", add_theme)
        .register_fn("theme-style", get_style)
        .register_fn("theme-set-style!", set_style)
        .register_fn("string->color", string_to_color);

    if generate_sources {
        configure_lsp_builtins("themes", &module);
    }

    engine.register_module(module);
}

#[derive(Clone)]
struct SteelTheme(Theme);
impl Custom for SteelTheme {}

fn theme_from_json_string(name: String, value: SteelVal) -> Result<SteelTheme, anyhow::Error> {
    // TODO: Really don't love this at all. The deserialization should be a bit more elegant
    let json_value = serde_json::Value::try_from(value)?;
    let value: toml::Value = serde_json::from_str(&serde_json::to_string(&json_value)?)?;

    let (mut theme, _) = Theme::from_toml(value);
    theme.set_name(name);
    Ok(SteelTheme(theme))
}

// Mutate the theme?
fn add_theme(cx: &mut Context, theme: SteelTheme) {
    cx.editor
        .user_defined_themes
        .insert(theme.0.name().to_owned(), theme.0);
}

fn get_style(theme: &SteelTheme, name: SteelString) -> helix_view::theme::Style {
    theme.0.get(name.as_str()).clone()
}

fn set_style(theme: &mut SteelTheme, name: String, style: helix_view::theme::Style) {
    theme.0.set(name, style)
}

fn string_to_color(string: SteelString) -> Result<Color, anyhow::Error> {
    // TODO: Don't expose this directly
    helix_view::theme::ThemePalette::string_to_rgb(string.as_str()).map_err(anyhow::Error::msg)
}

fn current_buffer_area(cx: &mut Context) -> Option<helix_view::graphics::Rect> {
    let focus = cx.editor.tree.focus;
    cx.editor.tree.view_id_area(focus)
}

fn load_editor_api(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/editor");

    // Types
    module.register_fn("Action/Load", || Action::Load);
    module.register_fn("Action/Replace", || Action::Replace);
    module.register_fn("Action/HorizontalSplit", || Action::HorizontalSplit);
    module.register_fn("Action/VerticalSplit", || Action::VerticalSplit);

    // Arity 0
    module.register_fn("editor-focus", cx_current_focus);
    module.register_fn("editor-mode", cx_get_mode);
    module.register_fn("cx->themes", get_themes);
    module.register_fn("editor-all-documents", cx_editor_all_documents);
    module.register_fn("cx->cursor", |cx: &mut Context| cx.editor.cursor());

    // Arity 1
    module.register_fn("editor->doc-id", cx_get_document_id);
    module.register_fn("editor-switch!", cx_switch);
    module.register_fn("editor-set-focus!", |cx: &mut Context, view_id: ViewId| {
        cx.editor.focus(view_id)
    });
    module.register_fn("editor-set-mode!", cx_set_mode);
    module.register_fn("editor-doc-in-view?", cx_is_document_in_view);
    module.register_fn("set-scratch-buffer-name!", set_scratch_buffer_name);
    module.register_fn("editor-doc-exists?", cx_document_exists);

    // Arity 2
    module.register_fn("editor-switch-action!", cx_switch_action);

    // Arity 1
    module.register_fn("editor->text", document_id_to_text);
    module.register_fn("editor-document->path", document_path);

    module.register_fn("set-editor-clip-right!", |cx: &mut Context, right: u16| {
        cx.editor.editor_clipping.right = Some(right);
    });
    module.register_fn("set-editor-clip-left!", |cx: &mut Context, left: u16| {
        cx.editor.editor_clipping.left = Some(left);
    });
    module.register_fn("set-editor-clip-top!", |cx: &mut Context, top: u16| {
        cx.editor.editor_clipping.top = Some(top);
    });
    module.register_fn(
        "set-editor-clip-bottom!",
        |cx: &mut Context, bottom: u16| {
            cx.editor.editor_clipping.bottom = Some(bottom);
        },
    );

    module.register_fn("editor-focused-buffer-area", current_buffer_area);

    if generate_sources {
        let mut builtin_editor_command_module =
            "(require-builtin helix/core/editor as helix.)".to_string();

        let mut template_function_type_constructor = |name: &str| {
            builtin_editor_command_module.push_str(&format!(
                r#"
(provide {})
(define ({})
    (helix.{}))
"#,
                name, name, name
            ));
        };

        template_function_type_constructor("Action/Load");
        template_function_type_constructor("Action/Replace");
        template_function_type_constructor("Action/HorizontalSplit");
        template_function_type_constructor("Action/VerticalSplit");

        let mut template_function_arity_0 = |name: &str| {
            builtin_editor_command_module.push_str(&format!(
                r#"
(provide {})
(define ({})
    (helix.{} *helix.cx*))
"#,
                name, name, name
            ));
        };

        template_function_arity_0("editor-focus");
        template_function_arity_0("editor-mode");
        template_function_arity_0("cx->themes");
        template_function_arity_0("editor-all-documents");
        template_function_arity_0("cx->cursor");
        template_function_arity_0("editor-focused-buffer-area");

        let mut template_function_arity_1 = |name: &str| {
            builtin_editor_command_module.push_str(&format!(
                r#"
(provide {})
(define ({} arg)
    (helix.{} *helix.cx* arg))
"#,
                name, name, name
            ));
        };

        template_function_arity_1("editor->doc-id");
        template_function_arity_1("editor-switch!");
        template_function_arity_1("editor-set-focus!");
        template_function_arity_1("editor-set-mode!");
        template_function_arity_1("editor-doc-in-view?");
        template_function_arity_1("set-scratch-buffer-name!");
        template_function_arity_1("editor-doc-exists?");
        template_function_arity_1("editor->text");
        template_function_arity_1("editor-document->path");

        template_function_arity_1("set-editor-clip-top!");
        template_function_arity_1("set-editor-clip-right!");
        template_function_arity_1("set-editor-clip-left!");
        template_function_arity_1("set-editor-clip-bottom!");

        let mut template_function_arity_2 = |name: &str| {
            builtin_editor_command_module.push_str(&format!(
                r#"
(provide {})
(define ({} arg1 arg2)
    (helix.{} *helix.cx* arg1 arg2))
"#,
                name, name, name
            ));
        };

        template_function_arity_2("editor-switch-action!");

        let mut target_directory = helix_runtime_search_path();

        if !target_directory.exists() {
            std::fs::create_dir_all(&target_directory).unwrap_or_else(|err| {
                panic!("Failed to create directory {:?}: {}", target_directory, err)
            });
            eprintln!("Created directory: {:?}", target_directory);
        }

        target_directory.push("editor.scm");

        std::fs::write(target_directory, builtin_editor_command_module).unwrap();
    }

    // Generate the lsp configuration
    if generate_sources {
        configure_lsp_builtins("editor", &module);
    }

    engine.register_module(module);
}

pub struct SteelScriptingEngine;

impl super::PluginSystem for SteelScriptingEngine {
    fn initialize(&self) {
        initialize_engine();
    }

    fn engine_name(&self) -> super::PluginSystemKind {
        super::PluginSystemKind::Steel
    }

    fn run_initialization_script(
        &self,
        cx: &mut Context,
        configuration: Arc<ArcSwapAny<Arc<Config>>>,
    ) {
        run_initialization_script(cx, configuration);
    }

    fn handle_keymap_event(
        &self,
        editor: &mut ui::EditorView,
        mode: Mode,
        cxt: &mut Context,
        event: KeyEvent,
    ) -> Option<KeymapResult> {
        SteelScriptingEngine::get_keymap_for_extension(cxt).and_then(|map| {
            if let steel::SteelVal::Custom(inner) = map {
                if let Some(underlying) =
                    steel::rvals::as_underlying_type::<EmbeddedKeyMap>(inner.read().as_ref())
                {
                    return Some(editor.keymaps.get_with_map(&underlying.0, mode, event));
                }
            }

            None
        })
    }

    fn call_function_by_name(&self, cx: &mut Context, name: &str, args: &[Cow<str>]) -> bool {
        if enter_engine(|x| x.global_exists(name)) {
            let args = args
                .iter()
                .map(|x| x.clone().into_steelval().unwrap())
                .collect::<Vec<_>>();

            if let Err(e) = enter_engine(|guard| {
                {
                    // Install the interrupt handler, in the event this thing
                    // is blocking for too long.
                    with_interrupt_handler(|| {
                        guard.with_mut_reference::<Context, Context>(cx).consume(
                            move |engine, arguments| {
                                let context = arguments[0].clone();
                                engine.update_value("*helix.cx*", context);

                                // TODO: Get rid of this clone
                                engine.call_function_by_name_with_args(name, args.clone())
                            },
                        )
                    })
                }
            }) {
                cx.editor.set_error(format!("{}", e));
            }
            true
        } else {
            false
        }
    }

    fn call_typed_command<'a>(
        &self,
        cx: &mut compositor::Context,
        input: &'a str,
        parts: &'a [&'a str],
        event: PromptEvent,
    ) -> bool {
        if enter_engine(|x| x.global_exists(parts[0])) {
            let shellwords = Shellwords::from(input);
            let args = shellwords.words();

            // We're finalizing the event - we actually want to call the function
            if event == PromptEvent::Validate {
                if let Err(e) = enter_engine(|guard| {
                    let args = args[1..]
                        .iter()
                        .map(|x| x.clone().into_steelval().unwrap())
                        .collect::<Vec<_>>();

                    let res = {
                        let mut ctx = Context {
                            register: None,
                            count: std::num::NonZeroUsize::new(1),
                            editor: cx.editor,
                            callback: Vec::new(),
                            on_next_key_callback: None,
                            jobs: cx.jobs,
                        };

                        // Install interrupt handler here during the duration
                        // of the function call
                        match with_interrupt_handler(|| {
                            guard
                                .with_mut_reference(&mut ctx)
                                .consume(move |engine, arguments| {
                                    let context = arguments[0].clone();
                                    engine.update_value("*helix.cx*", context);
                                    // TODO: Fix this clone
                                    engine.call_function_by_name_with_args(&parts[0], args.clone())
                                })
                        }) {
                            Ok(res) => {
                                cx.editor.set_status(res.to_string());
                                Ok(res)
                            }
                            Err(e) => Err(e),
                        }
                    };

                    res
                }) {
                    let mut ctx = Context {
                        register: None,
                        count: None,
                        editor: &mut cx.editor,
                        callback: Vec::new(),
                        on_next_key_callback: None,
                        jobs: &mut cx.jobs,
                    };

                    enter_engine(|x| present_error_inside_engine_context(&mut ctx, x, e));
                };
            }

            // Global exists
            true
        } else {
            // Global does not exist
            false
        }
    }

    fn get_doc_for_identifier(&self, ident: &str) -> Option<String> {
        enter_engine(|engine| get_doc_for_global(engine, ident))
    }

    // Just dump docs for all top level values?
    fn available_commands<'a>(&self) -> Vec<Cow<'a, str>> {
        enter_engine(|engine| {
            engine
                .readable_globals(*GLOBAL_OFFSET.get().unwrap())
                .iter()
                .map(|x| x.resolve().to_string().into())
                .collect()
        })
    }

    fn generate_sources(&self) {
        // Generate sources directly with a fresh engine
        let mut engine = Engine::new();
        configure_builtin_sources(&mut engine, true);
        // Generate documentation as well
        let target = helix_runtime_search_path();

        let mut writer = std::io::BufWriter::new(std::fs::File::create("steel-docs.md").unwrap());

        // Generate markdown docs
        steel_doc::walk_dir(&mut writer, target, &mut engine).unwrap();
    }
}

impl SteelScriptingEngine {
    // Attempt to fetch the keymap for the extension
    fn get_keymap_for_extension<'a>(cx: &'a mut Context) -> Option<SteelVal> {
        // Get the currently activated extension, also need to check the
        // buffer type.
        let extension = {
            let current_focus = cx.editor.tree.focus;
            let view = cx.editor.tree.get(current_focus);
            let doc = &view.doc;
            let current_doc = cx.editor.documents.get(doc);

            current_doc
                .and_then(|x| x.path())
                .and_then(|x| x.extension())
                .and_then(|x| x.to_str())
        };

        let doc_id = {
            let current_focus = cx.editor.tree.focus;
            let view = cx.editor.tree.get(current_focus);
            let doc = &view.doc;

            doc
        };

        if let Some(extension) = extension {
            if let SteelVal::Boxed(boxed_map) = BUFFER_OR_EXTENSION_KEYBINDING_MAP.clone() {
                if let SteelVal::HashMapV(map) = boxed_map.read().clone() {
                    if let Some(value) = map.get(&SteelVal::StringV(extension.into())) {
                        if let SteelVal::Custom(inner) = value {
                            if let Some(_) = steel::rvals::as_underlying_type::<EmbeddedKeyMap>(
                                inner.read().as_ref(),
                            ) {
                                return Some(value.clone());
                            }
                        }
                    }
                }
            }
        }

        if let SteelVal::Boxed(boxed_map) = REVERSE_BUFFER_MAP.clone() {
            if let SteelVal::HashMapV(map) = boxed_map.read().clone() {
                if let Some(label) = map.get(&SteelVal::IntV(document_id_to_usize(doc_id) as isize))
                {
                    if let SteelVal::Boxed(boxed_map) = BUFFER_OR_EXTENSION_KEYBINDING_MAP.clone() {
                        if let SteelVal::HashMapV(map) = boxed_map.read().clone() {
                            if let Some(value) = map.get(label) {
                                if let SteelVal::Custom(inner) = value {
                                    if let Some(_) =
                                        steel::rvals::as_underlying_type::<EmbeddedKeyMap>(
                                            inner.read().as_ref(),
                                        )
                                    {
                                        return Some(value.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }
}

pub fn initialize_engine() {
    enter_engine(|x| x.globals().first().copied());
}

pub fn present_error_inside_engine_context(cx: &mut Context, engine: &mut Engine, e: SteelErr) {
    cx.editor.set_error(e.to_string());

    let backtrace = engine.raise_error_to_string(e);

    let callback = async move {
        let call: job::Callback = Callback::EditorCompositor(Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor| {
                if let Some(backtrace) = backtrace {
                    let contents = ui::Markdown::new(
                        format!("```\n{}\n```", backtrace),
                        editor.syn_loader.clone(),
                    );
                    let popup = Popup::new("engine", contents).position(Some(
                        helix_core::Position::new(editor.cursor().0.unwrap_or_default().row, 2),
                    ));
                    compositor.replace_or_push("engine", popup);
                }
            },
        ));
        Ok(call)
    };
    cx.jobs.callback(callback);
}

// Key maps
#[derive(Clone, Debug)]
pub struct EmbeddedKeyMap(pub HashMap<Mode, KeyTrie>);
impl Custom for EmbeddedKeyMap {}

pub fn update_documentation(map: &mut EmbeddedKeyMap, docs: HashMap<String, String>) {
    let mut func = move |command: &mut MappableCommand| {
        if let Some(steel_doc) = docs.get(command.name()) {
            if let Some(doc) = command.doc_mut() {
                *doc = steel_doc.to_owned()
            }
        }
    };

    for trie in map.0.values_mut() {
        trie.apply(&mut func)
    }
}

// Will deep copy a value by default when using a value type
pub fn deep_copy_keymap(copied: EmbeddedKeyMap) -> EmbeddedKeyMap {
    copied
}

// Base level - no configuration
pub fn default_keymap() -> EmbeddedKeyMap {
    EmbeddedKeyMap(keymap::default())
}

// Completely empty, allow for overriding
pub fn empty_keymap() -> EmbeddedKeyMap {
    EmbeddedKeyMap(HashMap::default())
}

pub fn string_to_embedded_keymap(value: String) -> EmbeddedKeyMap {
    EmbeddedKeyMap(serde_json::from_str(&value).unwrap())
}

pub fn merge_keybindings(left: &mut EmbeddedKeyMap, right: EmbeddedKeyMap) {
    merge_keys(&mut left.0, right.0)
}

pub fn is_keymap(keymap: SteelVal) -> bool {
    if let SteelVal::Custom(underlying) = keymap {
        as_underlying_type::<EmbeddedKeyMap>(underlying.read().as_ref()).is_some()
    } else {
        false
    }
}

fn local_config_exists() -> bool {
    let local_helix = find_workspace().0.join(".helix");
    local_helix.join("helix.scm").exists() && local_helix.join("init.scm").exists()
}

fn preferred_config_path(file_name: &str) -> PathBuf {
    if local_config_exists() {
        find_workspace().0.join(".helix").join(file_name)
    } else {
        helix_loader::config_dir().join(file_name)
    }
}

pub fn helix_module_file() -> PathBuf {
    preferred_config_path("helix.scm")
}

pub fn steel_init_file() -> PathBuf {
    preferred_config_path("init.scm")
}

#[derive(Clone)]
struct HelixConfiguration {
    configuration: Arc<ArcSwapAny<Arc<Config>>>,
}

impl Custom for HelixConfiguration {}
// impl Custom for LineNumber {}

impl HelixConfiguration {
    fn load_config(&self) -> Config {
        (*self.configuration.load().clone()).clone()
    }

    fn store_config(&self, config: Config) {
        self.configuration.store(Arc::new(config));
    }

    // Overlay new keybindings
    fn keybindings(&self, keybindings: EmbeddedKeyMap) {
        let mut app_config = self.load_config();
        merge_keys(&mut app_config.keys, keybindings.0);
        self.store_config(app_config);
    }

    fn get_keybindings(&self) -> EmbeddedKeyMap {
        EmbeddedKeyMap(self.load_config().keys.clone())
    }

    fn scrolloff(&self, lines: usize) {
        let mut app_config = self.load_config();
        app_config.editor.scrolloff = lines;
        self.store_config(app_config);
    }

    fn scroll_lines(&self, lines: isize) {
        let mut app_config = self.load_config();
        app_config.editor.scroll_lines = lines;
        self.store_config(app_config);
    }

    fn mouse(&self, m: bool) {
        let mut app_config = self.load_config();
        app_config.editor.mouse = m;
        self.store_config(app_config);
    }

    fn shell(&self, shell: Vec<String>) {
        let mut app_config = self.load_config();
        app_config.editor.shell = shell;
        self.store_config(app_config);
    }

    // TODO: Make this a symbol, probably!
    fn line_number(&self, mode: LineNumber) {
        let mut app_config = self.load_config();
        app_config.editor.line_number = mode;
        self.store_config(app_config);
    }

    fn cursorline(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.cursorline = option;
        self.store_config(app_config);
    }

    fn cursorcolumn(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.cursorcolumn = option;
        self.store_config(app_config);
    }

    fn middle_click_paste(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.middle_click_paste = option;
        self.store_config(app_config);
    }

    fn auto_pairs(&self, config: AutoPairConfig) {
        let mut app_config = self.load_config();
        app_config.editor.auto_pairs = config;
        self.store_config(app_config);
    }

    fn auto_completion(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.auto_completion = option;
        self.store_config(app_config);
    }

    fn auto_format(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.auto_format = option;
        self.store_config(app_config);
    }

    fn auto_save(&self, option: AutoSave) {
        let mut app_config = self.load_config();
        app_config.editor.auto_save = option;
        self.store_config(app_config);
    }

    // TODO: Finish the auto save options!
    fn auto_save_after_delay_enable(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.auto_save.after_delay.enable = option;
        self.store_config(app_config);
    }

    // TODO: Finish diagnostic options!
    fn inline_diagnostics_cursor_line_enable(&self, severity: String) {
        let mut app_config = self.load_config();
        let severity = match severity.as_str() {
            "hint" => Severity::Hint,
            "info" => Severity::Info,
            "warning" => Severity::Warning,
            "error" => Severity::Error,
            _ => return,
        };
        app_config.editor.inline_diagnostics.cursor_line = DiagnosticFilter::Enable(severity);
        self.store_config(app_config);
    }

    fn inline_diagnostics_end_of_line_enable(&self, severity: String) {
        let mut app_config = self.load_config();
        let severity = match severity.as_str() {
            "hint" => Severity::Hint,
            "info" => Severity::Info,
            "warning" => Severity::Warning,
            "error" => Severity::Error,
            _ => return,
        };
        app_config.editor.end_of_line_diagnostics = DiagnosticFilter::Enable(severity);
        self.store_config(app_config);
    }

    fn text_width(&self, width: usize) {
        let mut app_config = self.load_config();
        app_config.editor.text_width = width;
        self.store_config(app_config);
    }

    fn idle_timeout(&self, ms: usize) {
        let mut app_config = self.load_config();
        app_config.editor.idle_timeout = Duration::from_millis(ms as u64);
        self.store_config(app_config);
    }

    fn completion_timeout(&self, ms: usize) {
        let mut app_config = self.load_config();
        app_config.editor.completion_timeout = Duration::from_millis(ms as u64);
        self.store_config(app_config);
    }

    fn preview_completion_insert(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.preview_completion_insert = option;
        self.store_config(app_config);
    }

    // TODO: Make sure this conversion works automatically
    fn completion_trigger_len(&self, length: u8) {
        let mut app_config = self.load_config();
        app_config.editor.completion_trigger_len = length;
        self.store_config(app_config);
    }

    fn completion_replace(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.completion_replace = option;
        self.store_config(app_config);
    }

    fn auto_info(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.auto_info = option;
        self.store_config(app_config);
    }

    fn cursor_shape(&self, config: CursorShapeConfig) {
        let mut app_config = self.load_config();
        app_config.editor.cursor_shape = config;
        self.store_config(app_config);
    }

    fn true_color(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.true_color = option;
        self.store_config(app_config);
    }

    fn insert_final_newline(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.insert_final_newline = option;
        self.store_config(app_config);
    }

    fn color_modes(&self, option: bool) {
        let mut app_config = self.load_config();
        app_config.editor.color_modes = option;
        self.store_config(app_config);
    }

    fn gutters(&self, config: GutterConfig) {
        let mut app_config = self.load_config();
        app_config.editor.gutters = config;
        self.store_config(app_config);
    }

    fn file_picker(&self, picker: FilePickerConfig) {
        let mut app_config = self.load_config();
        app_config.editor.file_picker = picker;
        self.store_config(app_config);
    }

    fn statusline(&self, config: StatusLineConfig) {
        let mut app_config = self.load_config();
        app_config.editor.statusline = config;
        self.store_config(app_config);
    }

    fn undercurl(&self, undercurl: bool) {
        let mut app_config = self.load_config();
        app_config.editor.undercurl = undercurl;
        self.store_config(app_config);
    }

    fn search(&self, config: SearchConfig) {
        let mut app_config = self.load_config();
        app_config.editor.search = config;
        self.store_config(app_config);
    }

    fn lsp(&self, config: LspConfig) {
        let mut app_config = self.load_config();
        app_config.editor.lsp = config;
        self.store_config(app_config);
    }

    fn terminal(&self, config: Option<TerminalConfig>) {
        let mut app_config = self.load_config();
        app_config.editor.terminal = config;
        self.store_config(app_config);
    }

    fn rulers(&self, cols: Vec<u16>) {
        let mut app_config = self.load_config();
        app_config.editor.rulers = cols;
        self.store_config(app_config);
    }

    fn whitespace(&self, config: WhitespaceConfig) {
        let mut app_config = self.load_config();
        app_config.editor.whitespace = config;
        self.store_config(app_config);
    }

    fn bufferline(&self, config: BufferLine) {
        let mut app_config = self.load_config();
        app_config.editor.bufferline = config;
        self.store_config(app_config);
    }

    fn indent_guides(&self, config: IndentGuidesConfig) {
        let mut app_config = self.load_config();
        app_config.editor.indent_guides = config;
        self.store_config(app_config);
    }

    fn soft_wrap(&self, config: SoftWrap) {
        let mut app_config = self.load_config();
        app_config.editor.soft_wrap = config;
        self.store_config(app_config);
    }

    fn workspace_lsp_roots(&self, roots: Vec<PathBuf>) {
        let mut app_config = self.load_config();
        app_config.editor.workspace_lsp_roots = roots;
        self.store_config(app_config);
    }

    fn default_line_ending(&self, config: LineEndingConfig) {
        let mut app_config = self.load_config();
        app_config.editor.default_line_ending = config;
        self.store_config(app_config);
    }

    fn smart_tab(&self, config: Option<SmartTabConfig>) {
        let mut app_config = self.load_config();
        app_config.editor.smart_tab = config;
        self.store_config(app_config);
    }
}

// Get doc from function ptr table, hack
fn get_doc_for_global(engine: &mut Engine, ident: &str) -> Option<String> {
    if engine.global_exists(ident) {
        let expr = format!("(#%function-ptr-table-get #%function-ptr-table {})", ident);
        Some(
            engine
                .run(expr)
                .ok()
                .and_then(|x| x.first().cloned())
                .and_then(|x| x.as_string().map(|x| x.as_str().to_string()))
                .unwrap_or_else(|| "Undocumented plugin command".to_string()),
        )
    } else {
        None
    }
}

/// Run the initialization script located at `$helix_config/init.scm`
/// This runs the script in the global environment, and does _not_ load it as a module directly
fn run_initialization_script(cx: &mut Context, configuration: Arc<ArcSwapAny<Arc<Config>>>) {
    log::info!("Loading init.scm...");

    let helix_module_path = helix_module_file();

    // TODO: Report the error from requiring the file!
    enter_engine(|guard| {
        // Embed the configuration so we don't have to communicate over the refresh
        // channel. The state is still stored within the `Application` struct, but
        // now we can just access it and signal a refresh of the config when we need to.
        guard.update_value(
            "*helix.config*",
            HelixConfiguration { configuration }
                .into_steelval()
                .unwrap(),
        );

        let res = guard.run_with_reference(
            cx,
            "*helix.cx*",
            &format!(r#"(require {:?})"#, helix_module_path.to_str().unwrap()),
        );

        // Present the error in the helix.scm loading
        if let Err(e) = res {
            present_error_inside_engine_context(cx, guard, e);
            return;
        }

        let helix_module_path = steel_init_file();

        // These contents need to be registered with the path?
        if let Ok(contents) = std::fs::read_to_string(&helix_module_path) {
            let res = guard.run_with_reference_from_path::<Context, Context>(
                cx,
                "*helix.cx*",
                &contents,
                helix_module_path,
            );

            match res {
                Ok(_) => {}
                Err(e) => present_error_inside_engine_context(cx, guard, e),
            }

            log::info!("Finished loading init.scm!")
        } else {
            log::info!("No init.scm found, skipping loading.")
        }
    });
}

impl Custom for PromptEvent {}

impl<'a> CustomReference for Context<'a> {}

steel::custom_reference!(Context<'a>);

fn get_themes(cx: &mut Context) -> Vec<String> {
    ui::completers::theme(cx.editor, "")
        .into_iter()
        .map(|x| x.1.content.to_string())
        .collect()
}

/// A dynamic component, used for rendering thing
impl Custom for compositor::EventResult {}

pub struct WrappedDynComponent {
    pub(crate) inner: Option<Box<dyn Component + Send + Sync + 'static>>,
}

impl Custom for WrappedDynComponent {}

pub struct BoxDynComponent {
    inner: Box<dyn Component>,
}

impl BoxDynComponent {
    pub fn new(inner: Box<dyn Component>) -> Self {
        Self { inner }
    }
}

impl Component for BoxDynComponent {
    fn handle_event(
        &mut self,
        _event: &helix_view::input::Event,
        _ctx: &mut compositor::Context,
    ) -> compositor::EventResult {
        self.inner.handle_event(_event, _ctx)
    }

    fn should_update(&self) -> bool {
        self.inner.should_update()
    }

    fn cursor(
        &self,
        _area: helix_view::graphics::Rect,
        _ctx: &Editor,
    ) -> (
        Option<helix_core::Position>,
        helix_view::graphics::CursorKind,
    ) {
        self.inner.cursor(_area, _ctx)
    }

    fn required_size(&mut self, _viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.inner.required_size(_viewport)
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }

    fn id(&self) -> Option<&'static str> {
        Some(self.inner.type_name())
    }

    fn name(&self) -> Option<&str> {
        self.inner.name()
    }

    fn render(
        &mut self,
        area: helix_view::graphics::Rect,
        frame: &mut tui::buffer::Buffer,
        ctx: &mut compositor::Context,
    ) {
        self.inner.render(area, frame, ctx)
    }
}

#[derive(Debug, Clone, Copy)]
struct OnModeSwitchEvent {
    old_mode: Mode,
    new_mode: Mode,
}

impl OnModeSwitchEvent {
    pub fn get_old_mode(&self) -> Mode {
        self.old_mode
    }

    pub fn get_new_mode(&self) -> Mode {
        self.new_mode
    }
}

impl Custom for OnModeSwitchEvent {}
impl Custom for MappableCommand {}

// Don't take the function name, just take the function itself?
fn register_hook(event_kind: String, callback_fn: SteelVal) -> steel::UnRecoverableResult {
    let rooted = callback_fn.as_rooted();

    match event_kind.as_str() {
        "on-mode-switch" => {
            register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
                // if enter_engine(|x| x.global_exists(&function_name)) {
                if let Err(e) = enter_engine(|guard| {
                    let minimized_event = OnModeSwitchEvent {
                        old_mode: event.old_mode,
                        new_mode: event.new_mode,
                    };

                    guard.with_mut_reference(event.cx).consume(|engine, args| {
                        let context = args[0].clone();
                        engine.update_value("*helix.cx*", context);

                        let mut args = vec![minimized_event.into_steelval().unwrap()];
                        // engine.call_function_by_name_with_args(&function_name, args)
                        engine.call_function_with_args_from_mut_slice(
                            rooted.value().clone(),
                            &mut args,
                        )
                    })
                }) {
                    event.cx.editor.set_error(e.to_string());
                }
                // }

                Ok(())
            });

            Ok(SteelVal::Void).into()
        }
        "post-insert-char" => {
            register_hook!(move |event: &mut PostInsertChar<'_, '_>| {
                // if enter_engine(|x| x.global_exists(&function_name)) {
                if let Err(e) = enter_engine(|guard| {
                    guard.with_mut_reference(event.cx).consume(|engine, args| {
                        let context = args[0].clone();
                        engine.update_value("*helix.cx*", context);

                        // args.push(event.c.into());
                        // engine.call_function_by_name_with_args(&function_name, vec![event.c.into()])

                        let mut args = vec![event.c.into()];

                        engine.call_function_with_args_from_mut_slice(
                            rooted.value().clone(),
                            &mut args,
                        )
                    })
                }) {
                    event.cx.editor.set_error(e.to_string());
                }
                // }

                Ok(())
            });

            Ok(SteelVal::Void).into()
        }
        // Register hook - on save?
        "post-command" => {
            register_hook!(move |event: &mut PostCommand<'_, '_>| {
                // if enter_engine(|x| x.global_exists(&function_name)) {
                if let Err(e) = enter_engine(|guard| {
                    guard.with_mut_reference(event.cx).consume(|engine, args| {
                        let context = args[0].clone();
                        engine.update_value("*helix.cx*", context);

                        let mut args = vec![event.command.name().into_steelval().unwrap()];

                        engine.call_function_with_args_from_mut_slice(
                            rooted.value().clone(),
                            &mut args,
                        )

                        // args.push(event.command.clone().into_steelval().unwrap());
                        // engine.call_function_by_name_with_args(
                        //     &function_name,
                        //     // Name?
                        //     vec![event.command.name().into_steelval().unwrap()],
                        // )
                    })
                }) {
                    event.cx.editor.set_error(e.to_string());
                }
                // }

                Ok(())
            });

            Ok(SteelVal::Void).into()
        }
        // Unimplemented!
        // "document-did-change" => {
        //     todo!()
        // }
        // "selection-did-change" => {
        //     todo!()
        // }
        _ => steelerr!(Generic => "Unable to register hook: Unknown event type: {}", event_kind)
            .into(),
    }
}

fn configure_lsp_globals() {
    if let Ok(steel_lsp_home) = std::env::var("STEEL_LSP_HOME") {
        let mut path = PathBuf::from(steel_lsp_home);
        path.push("_helix-global-builtins.scm");

        let mut output = String::new();

        let names = &[
            "*helix.cx*",
            "*helix.config*",
            "*helix.id*",
            "register-hook!",
            "log::info!",
            "fuzzy-match",
            "helix-find-workspace",
            "doc-id->usize",
            "new-component!",
            "acquire-context-lock",
            "SteelDynamicComponent?",
            "prompt",
            "picker",
            "Component::Text",
            "hx.create-directory",
        ];

        for value in names {
            use std::fmt::Write;
            writeln!(&mut output, "(#%register-global '{})", value).unwrap();
        }

        std::fs::write(path, output).unwrap();
    }
}

fn configure_lsp_builtins(name: &str, module: &BuiltInModule) {
    if let Ok(steel_lsp_home) = std::env::var("STEEL_LSP_HOME") {
        let mut path = PathBuf::from(steel_lsp_home);
        path.push(&format!("_helix-{}-builtins.scm", name));

        let mut output = String::new();

        output.push_str(&format!(
            r#"(define #%helix-{}-module (#%module "{}"))

(define (register-values module values)
  (map (lambda (ident) (#%module-add module (symbol->string ident) void)) values))
"#,
            name,
            module.name()
        ));

        output.push_str(&format!(r#"(register-values #%helix-{}-module '("#, name));

        for value in module.names() {
            use std::fmt::Write;
            writeln!(&mut output, "{}", value).unwrap();
        }

        output.push_str("))");

        std::fs::write(path, output).unwrap();
    }
}

fn load_rope_api(engine: &mut Engine, generate_sources: bool) {
    // Wrap the rope module?
    let rope_slice_module = rope_module();

    if generate_sources {
        configure_lsp_builtins("rope", &rope_slice_module);
    }

    engine.register_module(rope_slice_module);
}

// struct SteelEngine(Engine);

// impl SteelEngine {
//     pub fn call_function_by_name(
//         &mut self,
//         function_name: SteelString,
//         args: Vec<SteelVal>,
//     ) -> steel::rvals::Result<SteelVal> {
//         self.0
//             .call_function_by_name_with_args(function_name.as_str(), args.into_iter().collect())
//     }

//     /// Calling a function that was not defined in the runtime it was created in could
//     /// result in panics. You have been warned.
//     pub fn call_function(
//         &mut self,
//         function: SteelVal,
//         args: Vec<SteelVal>,
//     ) -> steel::rvals::Result<SteelVal> {
//         self.0
//             .call_function_with_args(function, args.into_iter().collect())
//     }

//     pub fn require_module(&mut self, module: SteelString) -> steel::rvals::Result<()> {
//         self.0.run(format!("(require \"{}\")", module)).map(|_| ())
//     }
// }

// impl Custom for SteelEngine {}

// static ENGINE_ID: AtomicUsize = AtomicUsize::new(0);

// thread_local! {
//     pub static ENGINE_MAP: SteelVal =
//         SteelVal::boxed(SteelVal::empty_hashmap());
// }

// Low level API work, these need to be loaded into the global environment in a predictable
// location, otherwise callbacks from plugin engines will not be handled properly!
// fn load_engine_api(engine: &mut Engine) {
//     fn id_to_engine(value: SteelVal) -> Option<SteelVal> {
//         if let SteelVal::Boxed(b) = ENGINE_MAP.with(|x| x.clone()) {
//             if let SteelVal::HashMapV(h) = b.read().clone() {
//                 return h.get(&value).cloned();
//             }
//         }

//         None
//     }

//     // module
//     engine
//         .register_fn("helix.controller.create-engine", || {
//             SteelEngine(configure_engine_impl(Engine::new()))
//         })
//         .register_fn("helix.controller.fresh-engine-id", || {
//             ENGINE_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
//         })
//         .register_fn(
//             "helix.controller.call-function-by-name",
//             SteelEngine::call_function_by_name,
//         )
//         .register_fn("helix.controller.call-function", SteelEngine::call_function)
//         .register_fn(
//             "helix.controller.require-module",
//             SteelEngine::require_module,
//         )
//         .register_value(
//             "helix.controller.engine-map",
//             ENGINE_MAP.with(|x| x.clone()),
//         )
//         .register_fn("helix.controller.id->engine", id_to_engine);
// }

fn load_misc_api(engine: &mut Engine, generate_sources: bool) {
    let mut module = BuiltInModule::new("helix/core/misc");

    let mut builtin_misc_module = if generate_sources {
        "(require-builtin helix/core/misc as helix.)".to_string()
    } else {
        "".to_string()
    };

    let mut template_function_arity_0 = |name: &str| {
        if generate_sources {
            builtin_misc_module.push_str(&format!(
                r#"
(provide {})
(define ({})
    (helix.{} *helix.cx*))
"#,
                name, name, name
            ));
        }
    };

    // Arity 0
    module.register_fn("hx.cx->pos", cx_pos_within_text);
    module.register_fn("mode-switch-old", OnModeSwitchEvent::get_old_mode);
    module.register_fn("mode-switch-new", OnModeSwitchEvent::get_new_mode);

    template_function_arity_0("hx.cx->pos");

    let mut template_function_arity_1 = |name: &str| {
        if generate_sources {
            builtin_misc_module.push_str(&format!(
                r#"
(provide {})
(define ({} arg)
    (helix.{} *helix.cx* arg))
"#,
                name, name, name
            ));
        }
    };

    // Arity 1
    module.register_fn("hx.custom-insert-newline", custom_insert_newline);
    module.register_fn("push-component!", push_component);
    module.register_fn("pop-last-component!", pop_last_component_by_name);
    module.register_fn("enqueue-thread-local-callback", enqueue_command);
    module.register_fn("set-status!", set_status);

    template_function_arity_1("pop-last-component!");
    template_function_arity_1("hx.custom-insert-newline");
    template_function_arity_1("push-component!");
    template_function_arity_1("enqueue-thread-local-callback");
    template_function_arity_1("set-status!");

    module.register_fn("send-lsp-command", send_arbitrary_lsp_command);
    if generate_sources {
        builtin_misc_module.push_str(
            r#"
    (provide send-lsp-command)
    ;;@doc
    ;; Send an lsp command. The `lsp-name` must correspond to an active lsp.
    ;; The method name corresponds to the method name that you'd expect to see
    ;; with the lsp, and the params can be passed as a hash table. The callback
    ;; provided will be called with whatever result is returned from the LSP,
    ;; deserialized from json to a steel value.
    ;; 
    ;; # Example
    ;; ```scheme
    ;; (define (view-crate-graph)
    ;;   (send-lsp-command "rust-analyzer"
    ;;                     "rust-analyzer/viewCrateGraph"
    ;;                     (hash "full" #f)
    ;;                     ;; Callback to run with the result
    ;;                     (lambda (result) (displayln result))))
    ;; ```
    (define (send-lsp-command lsp-name method-name params callback)
        (helix.send-lsp-command *helix.cx* lsp-name method-name params callback))
            "#,
        );
    }

    let mut template_function_arity_2 = |name: &str| {
        if generate_sources {
            builtin_misc_module.push_str(&format!(
                r#"
(provide {})
(define ({} arg1 arg2)
    (helix.{} *helix.cx* arg1 arg2))
"#,
                name, name, name
            ));
        }
    };

    // Arity 2
    module.register_fn(
        "enqueue-thread-local-callback-with-delay",
        enqueue_command_with_delay,
    );

    // Arity 2
    module.register_fn("helix-await-callback", await_value);

    template_function_arity_2("enqueue-thread-local-callback-with-delay");
    template_function_arity_2("helix-await-callback");

    if generate_sources {
        let mut target_directory = helix_runtime_search_path();

        if !target_directory.exists() {
            std::fs::create_dir(&target_directory).unwrap();
        }

        target_directory.push("misc.scm");

        std::fs::write(target_directory, builtin_misc_module).unwrap();
    }

    if generate_sources {
        configure_lsp_builtins("misc", &module);
    }

    engine.register_module(module);
}

pub fn helix_runtime_search_path() -> PathBuf {
    helix_loader::config_dir().join("helix")
}

pub fn configure_builtin_sources(engine: &mut Engine, generate_sources: bool) {
    load_editor_api(engine, generate_sources);
    load_theme_api(engine, generate_sources);
    load_configuration_api(engine, generate_sources);
    load_typed_commands(engine, generate_sources);
    load_static_commands(engine, generate_sources);
    // Note: This is going to be completely revamped soon.
    load_keymap_api(engine, KeyMapApi::new(), generate_sources);
    load_rope_api(engine, generate_sources);
    load_misc_api(engine, generate_sources);
    load_component_api(engine, generate_sources);

    // TODO: Remove this once all of the globals have been moved into their own modules
    if generate_sources {
        if std::env::var("STEEL_LSP_HOME").is_err() {
            eprintln!("Warning: STEEL_LSP_HOME is not set, so the steel lsp will not be configured with helix primitives");
        }
        configure_lsp_globals()
    }
}

fn configure_engine_impl(mut engine: Engine) -> Engine {
    log::info!("Loading engine!");

    engine.add_search_directory(helix_loader::config_dir());

    engine.register_value("*helix.cx*", SteelVal::Void);
    engine.register_value("*helix.config*", SteelVal::Void);
    engine.register_value(
        "*helix.id*",
        SteelVal::IntV(engine.engine_id().as_usize() as _),
    );

    // Don't generate source directories here
    configure_builtin_sources(&mut engine, false);

    // Hooks
    engine.register_fn("register-hook!", register_hook);
    engine.register_fn("log::info!", |message: String| log::info!("{}", message));

    engine.register_fn("fuzzy-match", |pattern: SteelString, items: SteelVal| {
        // Match against how they would be rendered?

        if let SteelVal::ListV(l) = items {
            let res = helix_core::fuzzy::fuzzy_match(
                pattern.as_str(),
                l.iter().filter_map(|x| x.as_string().map(|x| x.as_str())),
                false,
            );

            return res
                .into_iter()
                .map(|x| x.0.to_string().into())
                .collect::<Vec<SteelVal>>();
        }

        return Vec::new();
    });

    // Find the workspace
    engine.register_fn("helix-find-workspace", || {
        helix_core::find_workspace().0.to_str().unwrap().to_string()
    });

    engine.register_fn("doc-id->usize", document_id_to_usize);

    engine.register_fn("new-component!", SteelDynamicComponent::new_dyn);

    engine.register_fn(
        "acquire-context-lock",
        |callback_fn: SteelVal, place: Option<SteelVal>| {
            match (&callback_fn, &place) {
                (SteelVal::Closure(_), Some(SteelVal::CustomStruct(_))) => {}
                _ => {
                    steel::stop!(TypeMismatch => "acquire-context-lock expected a 
                        callback function and a task object")
                }
            }

            let rooted = callback_fn.as_rooted();
            let rooted_place = place.map(|x| x.as_rooted());

            let callback =
                move |editor: &mut Editor, _compositor: &mut Compositor, jobs: &mut job::Jobs| {
                    let mut ctx = Context {
                        register: None,
                        count: None,
                        editor,
                        callback: Vec::new(),
                        on_next_key_callback: None,
                        jobs,
                    };

                    let cloned_func = rooted.value();
                    let cloned_place = rooted_place.as_ref().map(|x| x.value());

                    enter_engine(|guard| {
                        if let Err(e) = guard
                            .with_mut_reference::<Context, Context>(&mut ctx)
                            // Block until the other thread is finished in its critical
                            // section...
                            .consume(move |engine, args| {
                                let context = args[0].clone();
                                engine.update_value("*helix.cx*", context);

                                if let Some(SteelVal::CustomStruct(s)) = cloned_place {
                                    let mutex = s.get_mut_index(0).unwrap();
                                    mutex_lock(&mutex).unwrap();
                                }

                                // Acquire lock, wait until its done
                                let result =
                                    engine.call_function_with_args(cloned_func.clone(), Vec::new());

                                if let Some(SteelVal::CustomStruct(s)) = cloned_place {
                                    match result {
                                        Ok(result) => {
                                            // Store the result of the callback so that the
                                            // next downstream user can handle it.
                                            s.set_index(2, result);
                                            s.set_index(1, SteelVal::BoolV(true));
                                            let mutex = s.get_mut_index(0).unwrap();
                                            mutex_unlock(&mutex).unwrap();
                                        }

                                        Err(e) => {
                                            return Err(e);
                                        }
                                    }
                                }

                                Ok(())
                            })
                        {
                            present_error_inside_engine_context(&mut ctx, guard, e);
                        }
                    })
                };
            job::dispatch_blocking_jobs(callback);

            Ok(())
        },
    );

    engine.register_fn("SteelDynamicComponent?", |object: SteelVal| {
        if let SteelVal::Custom(v) = object {
            if let Some(wrapped) = v.read().as_any_ref().downcast_ref::<BoxDynComponent>() {
                return wrapped.inner.as_any().is::<SteelDynamicComponent>();
            } else {
                false
            }
        } else {
            false
        }
    });

    engine.register_fn(
        "prompt",
        |prompt: String, callback_fn: SteelVal| -> WrappedDynComponent {
            let callback_fn_guard = callback_fn.as_rooted();

            let prompt = Prompt::new(
                prompt.into(),
                None,
                |_, _| Vec::new(),
                move |cx, input, prompt_event| {
                    log::info!("Calling dynamic prompt callback");

                    if prompt_event != PromptEvent::Validate {
                        return;
                    }

                    let mut ctx = Context {
                        register: None,
                        count: None,
                        editor: cx.editor,
                        callback: Vec::new(),
                        on_next_key_callback: None,
                        jobs: cx.jobs,
                    };

                    let cloned_func = callback_fn_guard.value();

                    with_interrupt_handler(|| {
                        enter_engine(|guard| {
                            if let Err(e) = guard
                                .with_mut_reference::<Context, Context>(&mut ctx)
                                .consume(move |engine, args| {
                                    let context = args[0].clone();

                                    engine.update_value("*helix.cx*", context);

                                    engine.call_function_with_args(
                                        cloned_func.clone(),
                                        vec![input.into_steelval().unwrap()],
                                    )
                                })
                            {
                                present_error_inside_engine_context(&mut ctx, guard, e);
                            }
                        })
                    })
                },
            );

            WrappedDynComponent {
                inner: Some(Box::new(prompt)),
            }
        },
    );

    engine.register_fn("picker", |values: Vec<String>| -> WrappedDynComponent {
        let columns = [PickerColumn::new(
            "path",
            |item: &PathBuf, root: &PathBuf| {
                item.strip_prefix(root)
                    .unwrap_or(item)
                    .to_string_lossy()
                    .into()
            },
        )];
        let cwd = helix_stdx::env::current_working_dir();

        let picker = ui::Picker::new(columns, 0, [], cwd, move |cx, path: &PathBuf, action| {
            if let Err(e) = cx.editor.open(path, action) {
                let err = if let Some(err) = e.source() {
                    format!("{}", err)
                } else {
                    format!("unable to open \"{}\"", path.display())
                };
                cx.editor.set_error(err);
            }
        })
        .with_preview(|_editor, path| Some((PathOrId::Path(path), None)));

        let injector = picker.injector();

        for file in values {
            if injector.push(PathBuf::from(file)).is_err() {
                break;
            }
        }

        WrappedDynComponent {
            inner: Some(Box::new(ui::overlay::overlaid(picker))),
        }
    });

    engine.register_fn("Component::Text", |contents: String| WrappedDynComponent {
        inner: Some(Box::new(crate::ui::Text::new(contents))),
    });

    // Create directory since we can't do that in the current state
    engine.register_fn("hx.create-directory", create_directory);

    engine
}

fn get_highlighted_text(cx: &mut Context) -> String {
    let (view, doc) = current_ref!(cx.editor);
    let text = doc.text().slice(..);
    doc.selection(view.id).primary().slice(text).to_string()
}

fn current_selection(cx: &mut Context) -> Selection {
    let (view, doc) = current_ref!(cx.editor);
    doc.selection(view.id).clone()
}

fn set_selection(cx: &mut Context, selection: Selection) {
    let (view, doc) = current!(cx.editor);
    doc.set_selection(view.id, selection)
}

fn current_line_number(cx: &mut Context) -> usize {
    let (view, doc) = current_ref!(cx.editor);
    helix_core::coords_at_pos(
        doc.text().slice(..),
        doc.selection(view.id)
            .primary()
            .cursor(doc.text().slice(..)),
    )
    .row
}

fn get_selection(cx: &mut Context) -> String {
    let (view, doc) = current_ref!(cx.editor);
    let text = doc.text().slice(..);

    let grapheme_start = doc.selection(view.id).primary().cursor(text);
    let grapheme_end = graphemes::next_grapheme_boundary(text, grapheme_start);

    if grapheme_start == grapheme_end {
        return "".into();
    }

    let grapheme = text.slice(grapheme_start..grapheme_end).to_string();

    let printable = grapheme.chars().fold(String::new(), |mut s, c| {
        match c {
            '\0' => s.push_str("\\0"),
            '\t' => s.push_str("\\t"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            _ => s.push(c),
        }

        s
    });

    printable
}

// TODO: Replace with eval-string
pub fn run_expression_in_engine(cx: &mut Context, text: String) -> anyhow::Result<()> {
    let callback = async move {
        let call: Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut job::Jobs)> = Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor, jobs: &mut job::Jobs| {
                let mut ctx = Context {
                    register: None,
                    count: None,
                    editor,
                    callback: Vec::new(),
                    on_next_key_callback: None,
                    jobs,
                };

                let output = enter_engine(|guard| {
                    guard
                        .with_mut_reference::<Context, Context>(&mut ctx)
                        .consume(move |engine, args| {
                            let context = args[0].clone();
                            engine.update_value("*helix.cx*", context);

                            engine.compile_and_run_raw_program(text.clone())
                        })
                });

                match output {
                    Ok(output) => {
                        let (output, _success) = (Tendril::from(format!("{:?}", output)), true);

                        let contents = ui::Markdown::new(
                            format!("```\n{}\n```", output),
                            editor.syn_loader.clone(),
                        );
                        let popup = Popup::new("engine", contents).position(Some(
                            helix_core::Position::new(editor.cursor().0.unwrap_or_default().row, 2),
                        ));
                        compositor.replace_or_push("engine", popup);
                    }
                    Err(e) => enter_engine(|x| present_error_inside_engine_context(&mut ctx, x, e)),
                }
            },
        );
        Ok(call)
    };
    cx.jobs.local_callback(callback);

    Ok(())
}

pub fn load_buffer(cx: &mut Context) -> anyhow::Result<()> {
    let (text, path) = {
        let (_, doc) = current!(cx.editor);

        let text = doc.text().to_string();
        let path = current_path(cx);

        (text, path)
    };

    let callback = async move {
        let call: Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut job::Jobs)> = Box::new(
            move |editor: &mut Editor, compositor: &mut Compositor, jobs: &mut job::Jobs| {
                let mut ctx = Context {
                    register: None,
                    count: None,
                    editor,
                    callback: Vec::new(),
                    on_next_key_callback: None,
                    jobs,
                };

                let output = enter_engine(|guard| {
                    guard
                        .with_mut_reference::<Context, Context>(&mut ctx)
                        .consume(move |engine, args| {
                            let context = args[0].clone();
                            engine.update_value("*helix.cx*", context);

                            match path.clone() {
                                Some(path) => engine.compile_and_run_raw_program_with_path(
                                    // TODO: Figure out why I have to clone this text here.
                                    text.clone(),
                                    PathBuf::from(path),
                                ),
                                None => engine.compile_and_run_raw_program(text.clone()),
                            }
                        })
                });

                match output {
                    Ok(output) => {
                        let (output, _success) = (Tendril::from(format!("{:?}", output)), true);

                        let contents = ui::Markdown::new(
                            format!("```\n{}\n```", output),
                            editor.syn_loader.clone(),
                        );
                        let popup = Popup::new("engine", contents).position(Some(
                            helix_core::Position::new(editor.cursor().0.unwrap_or_default().row, 2),
                        ));
                        compositor.replace_or_push("engine", popup);
                    }
                    Err(e) => enter_engine(|x| present_error_inside_engine_context(&mut ctx, x, e)),
                }
            },
        );
        Ok(call)
    };
    cx.jobs.local_callback(callback);

    Ok(())
}

fn get_helix_scm_path() -> String {
    helix_module_file().to_str().unwrap().to_string()
}

fn get_init_scm_path() -> String {
    steel_init_file().to_str().unwrap().to_string()
}

/// Get the current path! See if this can be done _without_ this function?
// TODO:
fn current_path(cx: &mut Context) -> Option<String> {
    let current_focus = cx.editor.tree.focus;
    let view = cx.editor.tree.get(current_focus);
    let doc = &view.doc;
    // Lifetime of this needs to be tied to the existing document
    let current_doc = cx.editor.documents.get(doc);
    current_doc.and_then(|x| x.path().and_then(|x| x.to_str().map(|x| x.to_string())))
}

fn set_scratch_buffer_name(cx: &mut Context, name: String) {
    let current_focus = cx.editor.tree.focus;
    let view = cx.editor.tree.get(current_focus);
    let doc = &view.doc;
    // Lifetime of this needs to be tied to the existing document
    let current_doc = cx.editor.documents.get_mut(doc);

    if let Some(current_doc) = current_doc {
        current_doc.name = Some(name);
    }
}

fn cx_current_focus(cx: &mut Context) -> helix_view::ViewId {
    cx.editor.tree.focus
}

fn cx_get_document_id(cx: &mut Context, view_id: helix_view::ViewId) -> DocumentId {
    cx.editor.tree.get(view_id).doc
}

fn document_id_to_text(cx: &mut Context, doc_id: DocumentId) -> Option<SteelRopeSlice> {
    cx.editor
        .documents
        .get(&doc_id)
        .map(|x| SteelRopeSlice::new(x.text().clone()))
}

fn cx_is_document_in_view(cx: &mut Context, doc_id: DocumentId) -> Option<helix_view::ViewId> {
    cx.editor
        .tree
        .traverse()
        .find(|(_, v)| v.doc == doc_id)
        .map(|(id, _)| id)
}

fn cx_document_exists(cx: &mut Context, doc_id: DocumentId) -> bool {
    cx.editor.documents.get(&doc_id).is_some()
}

fn document_path(cx: &mut Context, doc_id: DocumentId) -> Option<String> {
    cx.editor
        .documents
        .get(&doc_id)
        .and_then(|doc| doc.path().and_then(|x| x.to_str()).map(|x| x.to_string()))
}

fn cx_editor_all_documents(cx: &mut Context) -> Vec<DocumentId> {
    cx.editor.documents.keys().copied().collect()
}

fn cx_switch(cx: &mut Context, doc_id: DocumentId) {
    cx.editor.switch(doc_id, Action::VerticalSplit)
}

fn cx_switch_action(cx: &mut Context, doc_id: DocumentId, action: Action) {
    cx.editor.switch(doc_id, action)
}

fn cx_get_mode(cx: &mut Context) -> Mode {
    cx.editor.mode
}

fn cx_set_mode(cx: &mut Context, mode: Mode) {
    cx.editor.mode = mode
}

// Overlay the dynamic component, see what happens?
// Probably need to pin the values to this thread - wrap it in a shim which pins the value
// to this thread? - call methods on the thread local value?
fn push_component(cx: &mut Context, component: &mut WrappedDynComponent) {
    log::info!("Pushing dynamic component!");

    let inner = component.inner.take().unwrap();

    let callback = async move {
        let call: Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut job::Jobs)> = Box::new(
            move |_editor: &mut Editor, compositor: &mut Compositor, _| compositor.push(inner),
        );
        Ok(call)
    };
    cx.jobs.local_callback(callback);
}

fn pop_last_component_by_name(cx: &mut Context, name: SteelString) {
    let callback = async move {
        let call: Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut job::Jobs)> = Box::new(
            move |_editor: &mut Editor, compositor: &mut Compositor, _jobs: &mut job::Jobs| {
                compositor.remove_by_dynamic_name(&name);
            },
        );
        Ok(call)
    };
    cx.jobs.local_callback(callback);
}

fn set_status(cx: &mut Context, value: SteelVal) {
    cx.editor.set_status(value.to_string())
}

fn enqueue_command(cx: &mut Context, callback_fn: SteelVal) {
    let rooted = callback_fn.as_rooted();

    let callback = async move {
        let call: Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut job::Jobs)> = Box::new(
            move |editor: &mut Editor, _compositor: &mut Compositor, jobs: &mut job::Jobs| {
                let mut ctx = Context {
                    register: None,
                    count: None,
                    editor,
                    callback: Vec::new(),
                    on_next_key_callback: None,
                    jobs,
                };

                let cloned_func = rooted.value();

                enter_engine(|guard| {
                    if let Err(e) = guard
                        .with_mut_reference::<Context, Context>(&mut ctx)
                        .consume(move |engine, args| {
                            let context = args[0].clone();
                            engine.update_value("*helix.cx*", context);

                            engine.call_function_with_args(cloned_func.clone(), Vec::new())
                        })
                    {
                        present_error_inside_engine_context(&mut ctx, guard, e);
                    }
                })
            },
        );
        Ok(call)
    };
    cx.jobs.local_callback(callback);
}

// Apply arbitrary delay for update rate...
fn enqueue_command_with_delay(cx: &mut Context, delay: SteelVal, callback_fn: SteelVal) {
    let rooted = callback_fn.as_rooted();

    let callback = async move {
        let delay = delay.int_or_else(|| panic!("FIX ME")).unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(delay as u64)).await;

        let call: Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut job::Jobs)> = Box::new(
            move |editor: &mut Editor, _compositor: &mut Compositor, jobs: &mut job::Jobs| {
                let mut ctx = Context {
                    register: None,
                    count: None,
                    editor,
                    callback: Vec::new(),
                    on_next_key_callback: None,
                    jobs,
                };

                let cloned_func = rooted.value();

                enter_engine(|guard| {
                    if let Err(e) = guard
                        .with_mut_reference::<Context, Context>(&mut ctx)
                        .consume(move |engine, args| {
                            let context = args[0].clone();
                            engine.update_value("*helix.cx*", context);

                            engine.call_function_with_args(cloned_func.clone(), Vec::new())
                        })
                    {
                        present_error_inside_engine_context(&mut ctx, guard, e);
                    }
                })
            },
        );
        Ok(call)
    };
    cx.jobs.local_callback(callback);
}

// value _must_ be a future here. Otherwise awaiting will cause problems!
fn await_value(cx: &mut Context, value: SteelVal, callback_fn: SteelVal) {
    if !value.is_future() {
        return;
    }

    let rooted = callback_fn.as_rooted();

    let callback = async move {
        let future_value = value.as_future().unwrap().await;

        let call: Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut job::Jobs)> = Box::new(
            move |editor: &mut Editor, _compositor: &mut Compositor, jobs: &mut job::Jobs| {
                let mut ctx = Context {
                    register: None,
                    count: None,
                    editor,
                    callback: Vec::new(),
                    on_next_key_callback: None,
                    jobs,
                };

                let cloned_func = rooted.value();

                match future_value {
                    Ok(inner) => {
                        let callback = move |engine: &mut Engine, args: Vec<SteelVal>| {
                            let context = args[0].clone();
                            engine.update_value("*helix.cx*", context);

                            // args.push(inner);
                            engine.call_function_with_args(cloned_func.clone(), vec![inner])
                        };

                        enter_engine(|guard| {
                            if let Err(e) = guard
                                .with_mut_reference::<Context, Context>(&mut ctx)
                                .consume_once(callback)
                            {
                                present_error_inside_engine_context(&mut ctx, guard, e);
                            }
                        })
                    }
                    Err(e) => enter_engine(|x| present_error_inside_engine_context(&mut ctx, x, e)),
                }
            },
        );
        Ok(call)
    };
    cx.jobs.local_callback(callback);
}
// Check that we successfully created a directory?
fn create_directory(path: String) {
    let path = helix_stdx::path::canonicalize(&PathBuf::from(path));

    if path.exists() {
        return;
    } else {
        std::fs::create_dir(path).unwrap();
    }
}

pub fn cx_pos_within_text(cx: &mut Context) -> usize {
    let (view, doc) = current_ref!(cx.editor);

    let text = doc.text().slice(..);

    let selection = doc.selection(view.id).clone();

    let pos = selection.primary().cursor(text);

    pos
}

pub fn get_helix_cwd(_cx: &mut Context) -> Option<String> {
    helix_stdx::env::current_working_dir()
        .as_os_str()
        .to_str()
        .map(|x| x.into())
}

// Special newline...
pub fn custom_insert_newline(cx: &mut Context, indent: String) {
    let (view, doc) = current_ref!(cx.editor);

    // let rope = doc.text().clone();

    let text = doc.text().slice(..);

    let contents = doc.text();
    let selection = doc.selection(view.id).clone();
    let mut ranges = helix_core::SmallVec::with_capacity(selection.len());

    // TODO: this is annoying, but we need to do it to properly calculate pos after edits
    let mut global_offs = 0;

    let mut transaction =
        helix_core::Transaction::change_by_selection(contents, &selection, |range| {
            let pos = range.cursor(text);

            let prev = if pos == 0 {
                ' '
            } else {
                contents.char(pos - 1)
            };
            let curr = contents.get_char(pos).unwrap_or(' ');

            let current_line = text.char_to_line(pos);
            let line_is_only_whitespace = text
                .line(current_line)
                .chars()
                .all(|char| char.is_ascii_whitespace());

            let mut new_text = String::new();

            // If the current line is all whitespace, insert a line ending at the beginning of
            // the current line. This makes the current line empty and the new line contain the
            // indentation of the old line.
            let (from, to, local_offs) = if line_is_only_whitespace {
                let line_start = text.line_to_char(current_line);
                new_text.push_str(doc.line_ending.as_str());

                (line_start, line_start, new_text.chars().count())
            } else {
                // If we are between pairs (such as brackets), we want to
                // insert an additional line which is indented one level
                // more and place the cursor there
                let on_auto_pair = doc
                    .auto_pairs(cx.editor)
                    .and_then(|pairs| pairs.get(prev))
                    .map_or(false, |pair| pair.open == prev && pair.close == curr);

                let local_offs = if on_auto_pair {
                    let inner_indent = indent.clone() + doc.indent_style.as_str();
                    new_text.reserve_exact(2 + indent.len() + inner_indent.len());
                    new_text.push_str(doc.line_ending.as_str());
                    new_text.push_str(&inner_indent);
                    let local_offs = new_text.chars().count();
                    new_text.push_str(doc.line_ending.as_str());
                    new_text.push_str(&indent);
                    local_offs
                } else {
                    new_text.reserve_exact(1 + indent.len());
                    new_text.push_str(doc.line_ending.as_str());
                    new_text.push_str(&indent);
                    new_text.chars().count()
                };

                (pos, pos, local_offs)
            };

            let new_range = if doc.restore_cursor {
                // when appending, extend the range by local_offs
                Range::new(
                    range.anchor + global_offs,
                    range.head + local_offs + global_offs,
                )
            } else {
                // when inserting, slide the range by local_offs
                Range::new(
                    range.anchor + local_offs + global_offs,
                    range.head + local_offs + global_offs,
                )
            };

            // TODO: range replace or extend
            // range.replace(|range| range.is_empty(), head); -> fn extend if cond true, new head pos
            // can be used with cx.mode to do replace or extend on most changes
            ranges.push(new_range);
            global_offs += new_text.chars().count();

            (from, to, Some(new_text.into()))
        });

    transaction = transaction.with_selection(Selection::new(ranges, selection.primary_index()));

    let (view, doc) = current!(cx.editor);
    doc.apply(&transaction, view.id);
}

// fn search_in_directory(cx: &mut Context, directory: String) {
//     let buf = PathBuf::from(directory);
//     let search_path = expand_tilde(&buf);
//     let path = search_path.to_path_buf();
//     crate::commands::search_in_directory(cx, path);
// }

// TODO: Result should create unrecoverable result, and should have a special
// recoverable result - that way we can handle both, not one in particular
fn regex_selection(cx: &mut Context, regex: String) {
    if let Ok(regex) = helix_stdx::rope::Regex::new(&regex) {
        let (view, doc) = current!(cx.editor);
        let text = doc.text().slice(..);
        if let Some(selection) =
            helix_core::selection::select_on_matches(text, doc.selection(view.id), &regex)
        {
            doc.set_selection(view.id, selection);
        }
    }
}

fn replace_selection(cx: &mut Context, value: String) {
    let (view, doc) = current!(cx.editor);

    let selection = doc.selection(view.id);
    let transaction =
        helix_core::Transaction::change_by_selection(doc.text(), selection, |range| {
            if !range.is_empty() {
                (range.from(), range.to(), Some(value.to_owned().into()))
            } else {
                (range.from(), range.to(), None)
            }
        });

    doc.apply(&transaction, view.id);
}

// TODO: Remove this!
fn move_window_to_the_left(cx: &mut Context) {
    while cx
        .editor
        .tree
        .swap_split_in_direction(helix_view::tree::Direction::Left)
        .is_some()
    {}
}

// TODO: Remove this!
fn move_window_to_the_right(cx: &mut Context) {
    while cx
        .editor
        .tree
        .swap_split_in_direction(helix_view::tree::Direction::Right)
        .is_some()
    {}
}

fn send_arbitrary_lsp_command(
    cx: &mut Context,
    name: SteelString,
    command: SteelString,
    // Arguments - these will be converted to some json stuff
    json_argument: Option<SteelVal>,
    callback_fn: SteelVal,
) -> anyhow::Result<()> {
    let argument = json_argument.map(|x| serde_json::Value::try_from(x).unwrap());

    let (_view, doc) = current!(cx.editor);

    let language_server_id = anyhow::Context::context(
        doc.language_servers().find(|x| x.name() == name.as_str()),
        "Unable to find the language server specified!",
    )?
    .id();

    let future = match cx
        .editor
        .language_server_by_id(language_server_id)
        .and_then(|language_server| {
            language_server.non_standard_extension(command.to_string(), argument)
        }) {
        Some(future) => future,
        None => {
            // TODO: Come up with a better message once we check the capabilities for
            // the arbitrary thing you're trying to do, since for now the above actually
            // always returns a `Some`
            cx.editor.set_error(
                "Language server does not support whatever command you just tried to do",
            );
            return Ok(());
        }
    };

    let rooted = callback_fn.as_rooted();

    create_callback(cx, future, rooted)?;

    Ok(())
}

fn create_callback<T: TryInto<SteelVal, Error = SteelErr> + 'static>(
    cx: &mut Context,
    future: impl std::future::Future<Output = Result<T, helix_lsp::Error>> + 'static,
    rooted: steel::RootedSteelVal,
) -> Result<(), anyhow::Error> {
    let callback = async move {
        // Result of the future - this will be whatever we get back
        // from the lsp call
        let res = future.await?;

        let call: Box<dyn FnOnce(&mut Editor, &mut Compositor, &mut job::Jobs)> = Box::new(
            move |editor: &mut Editor, _compositor: &mut Compositor, jobs: &mut job::Jobs| {
                let mut ctx = Context {
                    register: None,
                    count: None,
                    editor,
                    callback: Vec::new(),
                    on_next_key_callback: None,
                    jobs,
                };

                let cloned_func = rooted.value();

                enter_engine(move |guard| match TryInto::<SteelVal>::try_into(res) {
                    Ok(result) => {
                        if let Err(e) = guard
                            .with_mut_reference::<Context, Context>(&mut ctx)
                            .consume(move |engine, args| {
                                let context = args[0].clone();
                                engine.update_value("*helix.cx*", context);

                                engine.call_function_with_args(
                                    cloned_func.clone(),
                                    vec![result.clone()],
                                )
                            })
                        {
                            present_error_inside_engine_context(&mut ctx, guard, e);
                        }
                    }
                    Err(e) => {
                        present_error_inside_engine_context(&mut ctx, guard, e);
                    }
                })
            },
        );
        Ok(call)
    };
    cx.jobs.local_callback(callback);
    Ok(())
}
