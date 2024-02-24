use std::sync::OnceLock;

use backtrace::Backtrace;

#[cfg(any(target_os = "linux", target_os = "android"))]
type FnSwapBuffers = fn(*const libc::c_void, *const libc::c_void) -> u32;
#[cfg(any(target_os = "linux", target_os = "android"))]
static SWAP_BUFFERS: OnceLock<plt_rs::FunctionHook<FnSwapBuffers>> = OnceLock::new();
#[cfg(any(target_os = "linux", target_os = "android"))]
fn hk_egl_swap_buffers(display: *const libc::c_void, surface: *const libc::c_void) -> u32 {
    // access global context
    let context = unsafe {
        CONTEXT.get_or_init(|| {
            // query for client dimensions
            let mut output_width = 0;
            if !egl::query_surface(
                display as *mut _,
                surface as *mut _,
                egl::EGL_WIDTH,
                &mut output_width,
            ) {
                panic!("Couldn't query surface for width")
            }

            let mut output_height = 0;
            if !egl::query_surface(
                display as *mut _,
                surface as *mut _,
                egl::EGL_HEIGHT,
                &mut output_height,
            ) {
                panic!("Couldn't query surface for height")
            }

            // create new context and stash old one
            let old_context = egl::get_current_context().expect("current context");

            let configs = egl::get_configs(display as *mut _, 1);
            let config = configs.configs;
            let render_context = egl::create_context(
                display as *mut _,
                config,
                egl::EGL_NO_CONTEXT,
                &[egl::EGL_CONTEXT_CLIENT_VERSION, 3, egl::EGL_NONE],
            )
            .expect("renderer");

            egl::make_current(
                display as *mut _,
                surface as *mut _,
                surface as *mut _,
                render_context,
            );

            let glow_context =
                glow::Context::from_loader_function(|i| egl::get_proc_address(i) as *const _);
            let glow_context = std::sync::Arc::new(glow_context);
            let egui_ctx = egui::Context::default();
            let painter =
                egui_glow::Painter::new(glow_context, "", None).expect("failed to create renderer");

            // restore old context after
            egl::make_current(
                display as *mut _,
                surface as *mut _,
                surface as *mut _,
                old_context,
            );

            PayloadContext {
                painter,
                egui_ctx,
                shapes: Default::default(),
                textures_delta: Default::default(),
                dimensions: [
                    output_width.try_into().unwrap(),
                    output_height.try_into().unwrap(),
                ],
                render_context,
            }
        });
        CONTEXT.get_mut().expect("context set")
    };

    // store old context
    let old_context = egl::get_current_context().expect("current context");
    egl::make_current(
        display as *mut _,
        surface as *mut _,
        surface as *mut _,
        context.render_context,
    );

    context.render();

    egl::make_current(
        display as *mut _,
        surface as *mut _,
        surface as *mut _,
        old_context,
    );

    // Swap the buffers and present our ui
    let swap_buffers = SWAP_BUFFERS.get().expect("swap buffers oncelock hook");
    (swap_buffers.cached_function())(display, surface)
}

// WINDOWS WGL
#[cfg(target_os = "windows")]
type FnSwapBuffers = fn(isize) -> i32;
#[cfg(target_os = "windows")]
static SWAP_BUFFERS: OnceLock<retour::GenericDetour<FnSwapBuffers>> = OnceLock::new();
#[cfg(target_os = "windows")]
fn hk_swap_buffers(hdc: isize) -> i32 {
    let hdc = windows::Win32::Graphics::Gdi::HDC(hdc);

    // access global context
    let context = unsafe {
        CONTEXT.get_or_init(|| {
            let window = windows::Win32::Graphics::Gdi::WindowFromDC(hdc);
            let mut dimensions = windows::Win32::Foundation::RECT::default();

            if !windows::Win32::UI::WindowsAndMessaging::GetClientRect(window, &mut dimensions)
                .as_bool()
            {
                panic!("Unable to acquire the window's dimensions");
            }

            let old_context = windows::Win32::Graphics::OpenGL::wglGetCurrentContext();
            let render_context =
                windows::Win32::Graphics::OpenGL::wglCreateContext(hdc).expect("create context");
            windows::Win32::Graphics::OpenGL::wglMakeCurrent(hdc, render_context);

            let glow_context = glow::Context::from_loader_function(|i| {
                match windows::Win32::Graphics::OpenGL::wglGetProcAddress(
                    windows::core::PCSTR::from_raw(i.as_ptr()),
                ) {
                    Some(ptr) => ptr as *const _,
                    None => std::ptr::null(),
                }
            });

            let glow_context = std::sync::Arc::new(glow_context);
            let egui_ctx = egui::Context::default();
            let painter =
                egui_glow::Painter::new(glow_context, "", None).expect("failed to create renderer");

            windows::Win32::Graphics::OpenGL::wglMakeCurrent(hdc, old_context);

            PayloadContext {
                painter,
                egui_ctx,
                shapes: Default::default(),
                textures_delta: Default::default(),
                dimensions: [
                    (dimensions.right - dimensions.left).try_into().unwrap(),
                    (dimensions.bottom - dimensions.top).try_into().unwrap(),
                ],
                render_context,
            }
        });
        CONTEXT.get_mut().expect("context set")
    };

    unsafe {
        let old_context = windows::Win32::Graphics::OpenGL::wglGetCurrentContext();
        windows::Win32::Graphics::OpenGL::wglMakeCurrent(hdc, context.render_context);
        context.render();
        windows::Win32::Graphics::OpenGL::wglMakeCurrent(hdc, old_context);
    }

    // Swap the buffers and present our ui
    let swap_buffers = SWAP_BUFFERS.get().expect("swap buffers oncelock hook");
    swap_buffers.call(hdc.0)
}

// COMMON RENDERING CONTEXT
static mut CONTEXT: OnceLock<PayloadContext> = OnceLock::new();
struct PayloadContext {
    painter: egui_glow::Painter,
    egui_ctx: egui::Context,

    shapes: Vec<egui::epaint::ClippedShape>,
    textures_delta: egui::TexturesDelta,

    dimensions: [u32; 2],

    #[cfg(any(target_os = "linux", target_os = "android"))]
    render_context: egl::EGLContext,

    #[cfg(target_os = "windows")]
    render_context: windows::Win32::Graphics::OpenGL::HGLRC,
}

impl PayloadContext {
    fn render(&mut self) {
        let egui::FullOutput {
            platform_output: _platform_output,
            textures_delta,
            pixels_per_point,
            viewport_output: _viewport_output,
            shapes,
        } = self.egui_ctx.run(Default::default(), |ui| {
            egui::SidePanel::left("my_side_panel").show(ui, |ui| {
                ui.heading("~~WTS Lobbies 200gpa EA~~~!");
            });
        });

        self.shapes = shapes;
        self.textures_delta.append(textures_delta);

        let shapes = std::mem::take(&mut self.shapes);
        let mut textures_delta = std::mem::take(&mut self.textures_delta);

        let clipped_primitives = self.egui_ctx.tessellate(shapes,pixels_per_point);
        self.painter.paint_and_update_textures(
            self.dimensions,
            self.egui_ctx.pixels_per_point(),
            &clipped_primitives,
            &textures_delta,
        );

    }
}

unsafe impl Send for PayloadContext {}
unsafe impl Sync for PayloadContext {}

fn initialize_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    let stdout_log = tracing_subscriber::fmt::layer().compact();
    let subscriber = tracing_subscriber::Registry::default().with(stdout_log);

    // Upgrade logger on android
    #[cfg(target_os = "android")]
    let subscriber = {
        let android_layer = tracing_android::layer("SellingLobbies")
            .expect("Unable to create android tracing layer");
        subscriber.with(android_layer)
    };

    tracing::subscriber::set_global_default(subscriber).expect("Unable to set global subscriber");

    // Add panic hook
    std::panic::set_hook(Box::new(|panic_info| {
        let backtrace = Backtrace::new();
        tracing::error!("{backtrace:?}");
        tracing::error!("{panic_info}");
    }));

    #[cfg(target_os = "android")]
    {
        tracing::warn!("Android logging enabled! Layer created");
    }
}

/// LINUX/ANDROID HOOKING / INJECTION PAYLOAD
#[cfg(target_os = "linux")]
fn find_link_map<'a>() -> Option<plt_rs::LinkMapView<'a>> {
    use plt_rs::LinkMapBacked;

    plt_rs::LinkMapView::from_executable()
}

#[cfg(target_os = "android")]
fn find_link_map<'a>() -> Option<plt_rs::LinkMapView<'a>> {
    use plt_rs::LinkMapBacked;
    use proc_maps::MapRange;

    fn find_mod_map_fuzzy(mod_name: &str, process: i32) -> Option<MapRange> {
        use proc_maps::get_process_maps;
        let maps = get_process_maps(process).expect("alive");
        maps.into_iter().find(|m| match m.filename() {
            Some(p) => p.to_str().map(|s| s.contains(mod_name)).unwrap_or(false),
            None => false,
        })
    }

    find_mod_map_fuzzy("liblibs.hal.system.rs2client.so", std::process::id() as i32)
        .map(|m| plt_rs::LinkMapView::from_address(m.start() + (m.size() / 2)))?
}

#[cfg(any(target_os = "linux", target_os = "android"))]
extern "C" fn init_thread(_input: *mut libc::c_void) -> *mut libc::c_void {
    use plt_rs::MutableLinkMap;

    initialize_logging();
    tracing::warn!("Entering tracing std usage");

    let link_map = match find_link_map() {
        Some(link_map) => link_map,
        None => {
            tracing::error!("Unable to locate app's link map");
            return std::ptr::null_mut();
        }
    };

    let mut map = MutableLinkMap::from_view(link_map);
    match map.hook::<FnSwapBuffers>("eglSwapBuffers", hk_egl_swap_buffers as *const _) {
        Ok(Some(h)) => {
            SWAP_BUFFERS.set(h).expect("swap buffers oncelock");
            true
        }
        Ok(None) => {
            tracing::error!("Unable to find the specified function!");
            false
        }
        Err(e) => {
            tracing::error!("An error occured during hooking the plt {e:#?}");
            false
        }
    };

    std::ptr::null_mut()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[ctor::ctor]
fn entry_point() {
    let _thid = unsafe {
        let mut thid = 0;
        let res = libc::pthread_create(
            &mut thid,
            std::ptr::null(),
            init_thread,
            std::ptr::null_mut(),
        );
        if res != 0 {
            panic!("Unable to create thread");
        }
        thid
    };
}

/// WINDOWS HOOKING / INJECTION PAYLOAD
#[cfg(target_os = "windows")]
unsafe extern "system" fn start_routine(_parameter: *mut std::ffi::c_void) -> u32 {
    initialize_logging();

    let wgl_proc_address = unsafe {
        let mod_handle =
            windows::Win32::System::LibraryLoader::GetModuleHandleA(windows::s!("opengl32.dll"))
                .expect("module handle");

        windows::Win32::System::LibraryLoader::GetProcAddress(
            mod_handle,
            windows::s!("wglSwapBuffers"),
        )
        .expect("wglSwapBuffers address")
    };

    let detour = retour::GenericDetour::<FnSwapBuffers>::new(
        std::mem::transmute(wgl_proc_address as *const () as *const _),
        hk_swap_buffers,
    )
    .expect("Failed to create swap buffers detour");
    tracing::info!("Initialized the GenericDetour");

    detour
        .enable()
        .expect("Failed to Enable swap buffers detour");
    tracing::info!("Enabled the generic detour");

    SWAP_BUFFERS.set(detour).expect("swap buffers oncelock");
    tracing::info!("Enabled the swap buffers detour");

    0
}

#[cfg(target_os = "windows")]
#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn DllMain(
    dll_module: usize,
    call_reason: u32,
    _reserved: usize,
) -> windows::Win32::Foundation::BOOL {
    if call_reason == windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH {
        let thread = unsafe {
            windows::Win32::System::Threading::CreateThread(
                None,
                0,
                Some(start_routine),
                Some(dll_module as *const usize as *const std::ffi::c_void),
                windows::Win32::System::Threading::THREAD_CREATION_FLAGS(0),
                None,
            )
        };

        match thread {
            Ok(_handle) => {
                tracing::info!("Created thread")
            }
            Err(e) => {
                panic!("Unable to create thread {e:?}")
            }
        }
    }

    true.into()
}
