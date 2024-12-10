fn main() {
    let libavutil = pkg_config::probe_library("libavutil").unwrap();
    let libavcodec = pkg_config::probe_library("libavcodec").unwrap();

    let mut includes = Vec::new();
    includes.extend_from_slice(&libavutil.include_paths[..]);
    includes.extend_from_slice(&libavcodec.include_paths[..]);

    let functions = &[
        "av_buffer_ref",
        "av_buffer_unref",
        "av_dict_free",
        "av_dict_set",
        "av_frame_alloc",
        "av_frame_free",
        "av_free",
        "av_get_pix_fmt_name",
        "av_hwdevice_ctx_alloc",
        "av_hwdevice_ctx_init",
        "av_hwdevice_get_hwframe_constraints",
        "av_hwframe_ctx_alloc",
        "av_hwframe_ctx_init",
        "av_hwframe_get_buffer",
        "av_log_default_callback",
        "av_log_set_callback",
        "av_log_set_level",
        "av_malloc",
        "av_new_packet",
        "av_packet_alloc",
        "av_packet_free",
        "av_strerror",
        "avcodec_alloc_context3",
        "avcodec_align_dimensions2",
        "avcodec_find_decoder_by_name",
        "avcodec_find_encoder_by_name",
        "avcodec_free_context",
        "avcodec_get_hw_frames_parameters",
        "avcodec_open2",
        "avcodec_receive_frame",
        "avcodec_receive_packet",
        "avcodec_send_frame",
        "avcodec_send_packet",
    ];

    let types = &[
        "AVFrame",
        "AVHWDeviceContext",
        "AVHWFramesContext",
        "AVPacket",
        "AVRational",
        "AVVkFrame",
        "AVVulkanDeviceContext",
        "AVVulkanFramesContext",
    ];

    let vars = &[
        "AV_LOG_TRACE",
        "AV_LOG_VERBOSE",
        "AV_LOG_INFO",
        "AV_LOG_WARNING",
        "AV_NUM_DATA_POINTERS",
    ];

    let mut bindings = bindgen::Builder::default()
        .clang_args(
            includes
                .into_iter()
                .map(|x| format!("-I{}", x.to_string_lossy())),
        )
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .rust_target(bindgen::RustTarget::Stable_1_77)
        .dynamic_library_name("ffmpeg")
        .dynamic_link_require_all(true);
    for f in functions {
        bindings = bindings.allowlist_function(f);
    }
    for t in types {
        bindings = bindings.allowlist_type(t);
    }
    for v in vars {
        bindings = bindings.allowlist_var(v);
    }

    let builder = bindings.generate().unwrap();
    let out_path = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    builder.write_to_file(out_path.join("bindings.rs")).unwrap()
}
