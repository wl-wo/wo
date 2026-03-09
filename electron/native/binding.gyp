{
  "targets": [
    {
      "target_name": "wo_dmabuf",
      "sources": [
        "wo_dmabuf.cc"
      ],
      "include_dirs": [
        "<!@(node -p \"require('node-addon-api').include\")"
      ],
      "defines": [
        "NAPI_DISABLE_CPP_EXCEPTIONS"
      ],
      "cflags_cc": [
        "-std=c++17",
        "-Wall",
        "-Wextra"
      ],
      "libraries": [
        "-lgbm",
        "-ldrm",
        "-lEGL",
        "-lGL"
      ],
      "include_dirs+": [
        "/usr/include/libdrm"
      ]
    }
  ]
}
