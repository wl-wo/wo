#include <napi.h>

#include <cstdint>
#include <cstring>
#include <map>
#include <mutex>
#include <stdexcept>
#include <string>

// Linux / GBM / DRM
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <drm/drm_fourcc.h>
#include <gbm.h>

static int g_drm_fd = -1;
static gbm_device *g_gbm = nullptr;
static std::mutex g_gbm_mutex;

static EGLDisplay g_egl_display = EGL_NO_DISPLAY;
static EGLContext g_egl_context = EGL_NO_CONTEXT;
static std::mutex g_egl_mutex;

typedef EGLImageKHR (*PFNEGLCREATEIMAGEKHRPROC)(EGLDisplay, EGLContext, EGLenum, EGLClientBuffer, const EGLint*);
typedef EGLBoolean (*PFNEGLDESTROYIMAGEKHRPROC)(EGLDisplay, EGLImageKHR);
typedef void (*PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)(GLenum, GLeglImageOES);

static PFNEGLCREATEIMAGEKHRPROC eglCreateImageKHR_fn = nullptr;
static PFNEGLDESTROYIMAGEKHRPROC eglDestroyImageKHR_fn = nullptr;
static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC glEGLImageTargetTexture2DOES_fn = nullptr;

struct TextureEntry {
  GLuint texture;
  EGLImageKHR image;
  int width;
  int height;
};

static std::map<std::string, TextureEntry> g_texture_cache;
static std::mutex g_texture_cache_mutex;

static bool ensure_gbm(const std::string &drm_device = "/dev/dri/renderD128") {
  std::lock_guard<std::mutex> lk(g_gbm_mutex);
  if (g_gbm)
    return true;

  g_drm_fd = ::open(drm_device.c_str(), O_RDWR | O_CLOEXEC);
  if (g_drm_fd < 0)
    return false;

  g_gbm = gbm_create_device(g_drm_fd);
  return g_gbm != nullptr;
}

static bool ensure_egl() {
  std::lock_guard<std::mutex> lk(g_egl_mutex);
  if (g_egl_display != EGL_NO_DISPLAY)
    return true;

  if (!ensure_gbm())
    return false;

  g_egl_display = eglGetDisplay((EGLNativeDisplayType)g_gbm);
  if (g_egl_display == EGL_NO_DISPLAY)
    return false;

  EGLint major, minor;
  if (!eglInitialize(g_egl_display, &major, &minor)) {
    g_egl_display = EGL_NO_DISPLAY;
    return false;
  }

  eglCreateImageKHR_fn = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
  eglDestroyImageKHR_fn = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
  glEGLImageTargetTexture2DOES_fn = (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");

  if (!eglCreateImageKHR_fn || !eglDestroyImageKHR_fn || !glEGLImageTargetTexture2DOES_fn)
    return false;

  if (!eglBindAPI(EGL_OPENGL_ES_API))
    return false;

  EGLint config_attribs[] = {
    EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
    EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
    EGL_RED_SIZE, 8,
    EGL_GREEN_SIZE, 8,
    EGL_BLUE_SIZE, 8,
    EGL_ALPHA_SIZE, 8,
    EGL_NONE
  };

  EGLConfig config;
  EGLint num_configs;
  if (!eglChooseConfig(g_egl_display, config_attribs, &config, 1, &num_configs) || num_configs < 1)
    return false;

  EGLint context_attribs[] = {
    EGL_CONTEXT_CLIENT_VERSION, 2,
    EGL_NONE
  };

  g_egl_context = eglCreateContext(g_egl_display, config, EGL_NO_CONTEXT, context_attribs);
  if (g_egl_context == EGL_NO_CONTEXT)
    return false;

  EGLint pbuffer_attribs[] = {
    EGL_WIDTH, 1,
    EGL_HEIGHT, 1,
    EGL_NONE
  };

  EGLSurface pbuffer = eglCreatePbufferSurface(g_egl_display, config, pbuffer_attribs);
  if (pbuffer == EGL_NO_SURFACE) {
    eglDestroyContext(g_egl_display, g_egl_context);
    g_egl_context = EGL_NO_CONTEXT;
    return false;
  }

  if (!eglMakeCurrent(g_egl_display, pbuffer, pbuffer, g_egl_context)) {
    eglDestroySurface(g_egl_display, pbuffer);
    eglDestroyContext(g_egl_display, g_egl_context);
    g_egl_context = EGL_NO_CONTEXT;
    return false;
  }

  return true;
}

struct BufEntry {
  gbm_bo *bo;
  int dmabuf_fd;
};

static std::map<uint32_t, BufEntry> g_bufs;
static std::mutex g_bufs_mutex;
static uint32_t g_next_token = 1;

static uint32_t store_buf(gbm_bo *bo, int fd) {
  std::lock_guard<std::mutex> lk(g_bufs_mutex);
  uint32_t tok = g_next_token++;
  g_bufs[tok] = {bo, fd};
  return tok;
}

static void release_buf(uint32_t tok) {
  std::lock_guard<std::mutex> lk(g_bufs_mutex);
  auto it = g_bufs.find(tok);
  if (it == g_bufs.end())
    return;
  ::close(it->second.dmabuf_fd);
  gbm_bo_destroy(it->second.bo);
  g_bufs.erase(it);
}

Napi::Value Init(const Napi::CallbackInfo &info) {
  Napi::Env env = info.Env();
  std::string dev = "/dev/dri/renderD128";
  if (info.Length() >= 1 && info[0].IsString())
    dev = info[0].As<Napi::String>().Utf8Value();

  if (!ensure_gbm(dev)) {
    Napi::Error::New(env, "Failed to open GBM device: " + dev)
        .ThrowAsJavaScriptException();
  }
  return env.Undefined();
}

Napi::Value ImportRgba(const Napi::CallbackInfo &info) {
  Napi::Env env = info.Env();

  if (info.Length() < 4) {
    Napi::TypeError::New(env, "importRgba(buf, w, h, stride)")
        .ThrowAsJavaScriptException();
    return env.Null();
  }

  auto pixel_buf = info[0].As<Napi::Buffer<uint8_t>>();
  uint32_t w = info[1].As<Napi::Number>().Uint32Value();
  uint32_t h = info[2].As<Napi::Number>().Uint32Value();
  uint32_t src_stride = info[3].As<Napi::Number>().Uint32Value();

  ensure_gbm();
  if (!g_gbm) {
    Napi::Error::New(env, "GBM device not initialised")
        .ThrowAsJavaScriptException();
    return env.Null();
  }

  gbm_bo *bo = nullptr;

  bo = gbm_bo_create_with_modifiers2(g_gbm, w, h, GBM_FORMAT_ARGB8888,
                                     (const uint64_t[]){DRM_FORMAT_MOD_LINEAR},
                                     1,
                                     GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);

  if (!bo) {
    bo = gbm_bo_create(g_gbm, w, h, GBM_FORMAT_ARGB8888,
                       GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
  }

  if (!bo) {
    bo = gbm_bo_create(g_gbm, w, h, GBM_FORMAT_ARGB8888, GBM_BO_USE_WRITE);
  }

  if (!bo) {
    Napi::Error::New(
        env, "gbm_bo_create failed: all buffer creation methods exhausted")
        .ThrowAsJavaScriptException();
    return env.Null();
  }

  void *map_data = nullptr;
  uint32_t dst_stride = 0;
  void *ptr =
      gbm_bo_map(bo, 0, 0, w, h, GBM_BO_TRANSFER_WRITE, &dst_stride, &map_data);
  if (!ptr) {
    gbm_bo_destroy(bo);
    Napi::Error::New(env, "gbm_bo_map failed").ThrowAsJavaScriptException();
    return env.Null();
  }

  const uint8_t *src = pixel_buf.Data();
  uint8_t *dst = static_cast<uint8_t *>(ptr);
  uint32_t row_bytes = std::min(src_stride, dst_stride);
  for (uint32_t y = 0; y < h; ++y)
    std::memcpy(dst + y * dst_stride, src + y * src_stride, row_bytes);

  gbm_bo_unmap(bo, map_data);

  int owned_fd = gbm_bo_get_fd(bo);
  if (owned_fd < 0) {
    gbm_bo_destroy(bo);
    Napi::Error::New(env, "gbm_bo_get_fd failed").ThrowAsJavaScriptException();
    return env.Null();
  }

  uint64_t mod = gbm_bo_get_modifier(bo);
  uint32_t mod_hi = static_cast<uint32_t>(mod >> 32);
  uint32_t mod_lo = static_cast<uint32_t>(mod & 0xFFFFFFFF);
  uint32_t token = store_buf(bo, owned_fd);

  Napi::Object result = Napi::Object::New(env);
  result.Set("fd", Napi::Number::New(env, owned_fd));
  result.Set("offset", Napi::Number::New(env, 0));
  result.Set("stride", Napi::Number::New(env, dst_stride));
  result.Set("modifier_hi", Napi::Number::New(env, mod_hi));
  result.Set("modifier_lo", Napi::Number::New(env, mod_lo));
  result.Set("token", Napi::Number::New(env, token));
  return result;
}

Napi::Value ReleaseBuffer(const Napi::CallbackInfo &info) {
  if (info.Length() >= 1 && info[0].IsNumber()) {
    uint32_t tok = info[0].As<Napi::Number>().Uint32Value();
    release_buf(tok);
  }
  return info.Env().Undefined();
}

Napi::Value SendFd(const Napi::CallbackInfo &info) {
  Napi::Env env = info.Env();
  if (info.Length() < 2) {
    Napi::TypeError::New(env, "sendFd(socketFd, dmabufFd)")
        .ThrowAsJavaScriptException();
    return env.Undefined();
  }

  int sock_fd = info[0].As<Napi::Number>().Int32Value();
  int dmabuf_fd = info[1].As<Napi::Number>().Int32Value();

  char dummy = 0;
  struct iovec iov = {&dummy, 1};

  // control message for SCM_RIGHTS.
  char cmsg_buf[CMSG_SPACE(sizeof(int))];
  std::memset(cmsg_buf, 0, sizeof(cmsg_buf));

  struct msghdr msg = {};
  msg.msg_iov = &iov;
  msg.msg_iovlen = 1;
  msg.msg_control = cmsg_buf;
  msg.msg_controllen = sizeof(cmsg_buf);

  struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
  cmsg->cmsg_level = SOL_SOCKET;
  cmsg->cmsg_type = SCM_RIGHTS;
  cmsg->cmsg_len = CMSG_LEN(sizeof(int));
  std::memcpy(CMSG_DATA(cmsg), &dmabuf_fd, sizeof(int));

  ssize_t sent = ::sendmsg(sock_fd, &msg, MSG_NOSIGNAL);
  if (sent < 0) {
    Napi::Error::New(env, std::string("sendmsg failed: ") + ::strerror(errno))
        .ThrowAsJavaScriptException();
  }
  return env.Undefined();
}

Napi::Value RecvFd(const Napi::CallbackInfo &info) {
  Napi::Env env = info.Env();
  if (info.Length() < 1) {
    Napi::TypeError::New(env, "recvFd(socketFd)").ThrowAsJavaScriptException();
    return env.Undefined();
  }

  int sock_fd = info[0].As<Napi::Number>().Int32Value();

  char dummy = 0;
  struct iovec iov = {&dummy, 1};

  char cmsg_buf[CMSG_SPACE(sizeof(int))];
  std::memset(cmsg_buf, 0, sizeof(cmsg_buf));

  struct msghdr msg = {};
  msg.msg_iov = &iov;
  msg.msg_iovlen = 1;
  msg.msg_control = cmsg_buf;
  msg.msg_controllen = sizeof(cmsg_buf);

  ssize_t n = ::recvmsg(sock_fd, &msg, MSG_CMSG_CLOEXEC);
  if (n < 0) {
    Napi::Error::New(env, std::string("recvmsg failed: ") + ::strerror(errno))
        .ThrowAsJavaScriptException();
    return Napi::Number::New(env, -1);
  }

  for (struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg); cmsg != nullptr;
       cmsg = CMSG_NXTHDR(&msg, cmsg)) {
    if (cmsg->cmsg_level == SOL_SOCKET && cmsg->cmsg_type == SCM_RIGHTS) {
      int received_fd = -1;
      std::memcpy(&received_fd, CMSG_DATA(cmsg), sizeof(int));
      return Napi::Number::New(env, received_fd);
    }
  }

  return Napi::Number::New(env, -1);
}

Napi::Value ImportDmabufTexture(const Napi::CallbackInfo &info) {
  Napi::Env env = info.Env();
  
  if (info.Length() < 5) {
    Napi::TypeError::New(env, "importDmabufTexture(windowName, fd, width, height, format)")
        .ThrowAsJavaScriptException();
    return env.Undefined();
  }

  std::string windowName = info[0].As<Napi::String>().Utf8Value();
  int fd = info[1].As<Napi::Number>().Int32Value();
  int width = info[2].As<Napi::Number>().Int32Value();
  int height = info[3].As<Napi::Number>().Int32Value();
  uint32_t format = info[4].As<Napi::Number>().Uint32Value();

  if (!ensure_egl()) {
    Napi::Error::New(env, "Failed to initialize EGL").ThrowAsJavaScriptException();
    return env.Undefined();
  }

  std::lock_guard<std::mutex> lk(g_texture_cache_mutex);

  auto it = g_texture_cache.find(windowName);
  if (it != g_texture_cache.end()) {
    if (it->second.image != EGL_NO_IMAGE_KHR) {
      eglDestroyImageKHR_fn(g_egl_display, it->second.image);
    }
    if (it->second.texture != 0) {
      glDeleteTextures(1, &it->second.texture);
    }
    g_texture_cache.erase(it);
  }

  EGLint attribs[] = {
    EGL_WIDTH, width,
    EGL_HEIGHT, height,
    EGL_LINUX_DRM_FOURCC_EXT, (EGLint)format,
    EGL_DMA_BUF_PLANE0_FD_EXT, fd,
    EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
    EGL_DMA_BUF_PLANE0_PITCH_EXT, width * 4,
    EGL_NONE
  };

  EGLImageKHR image = eglCreateImageKHR_fn(
    g_egl_display,
    EGL_NO_CONTEXT,
    EGL_LINUX_DMA_BUF_EXT,
    (EGLClientBuffer)nullptr,
    attribs
  );

  if (image == EGL_NO_IMAGE_KHR) {
    Napi::Error::New(env, "eglCreateImageKHR failed").ThrowAsJavaScriptException();
    return env.Undefined();
  }

  GLuint texture = 0;
  glGenTextures(1, &texture);
  glBindTexture(GL_TEXTURE_2D, texture);
  glEGLImageTargetTexture2DOES_fn(GL_TEXTURE_2D, (GLeglImageOES)image);
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
  glBindTexture(GL_TEXTURE_2D, 0);

  g_texture_cache[windowName] = {texture, image, width, height};

  Napi::Object result = Napi::Object::New(env);
  result.Set("texture", Napi::Number::New(env, texture));
  result.Set("width", Napi::Number::New(env, width));
  result.Set("height", Napi::Number::New(env, height));
  return result;
}

Napi::Value GetTexture(const Napi::CallbackInfo &info) {
  Napi::Env env = info.Env();
  
  if (info.Length() < 1) {
    Napi::TypeError::New(env, "getTexture(windowName)").ThrowAsJavaScriptException();
    return env.Undefined();
  }

  std::string windowName = info[0].As<Napi::String>().Utf8Value();

  std::lock_guard<std::mutex> lk(g_texture_cache_mutex);
  auto it = g_texture_cache.find(windowName);
  if (it == g_texture_cache.end()) {
    return env.Null();
  }

  Napi::Object result = Napi::Object::New(env);
  result.Set("texture", Napi::Number::New(env, it->second.texture));
  result.Set("width", Napi::Number::New(env, it->second.width));
  result.Set("height", Napi::Number::New(env, it->second.height));
  return result;
}

Napi::Value ReleaseTexture(const Napi::CallbackInfo &info) {
  Napi::Env env = info.Env();
  
  if (info.Length() < 1) {
    Napi::TypeError::New(env, "releaseTexture(windowName)").ThrowAsJavaScriptException();
    return env.Undefined();
  }

  std::string windowName = info[0].As<Napi::String>().Utf8Value();

  std::lock_guard<std::mutex> lk(g_texture_cache_mutex);
  auto it = g_texture_cache.find(windowName);
  if (it != g_texture_cache.end()) {
    if (it->second.image != EGL_NO_IMAGE_KHR) {
      eglDestroyImageKHR_fn(g_egl_display, it->second.image);
    }
    if (it->second.texture != 0) {
      glDeleteTextures(1, &it->second.texture);
    }
    g_texture_cache.erase(it);
  }

  return env.Undefined();
}

Napi::Value MmapFd(const Napi::CallbackInfo &info) {
  Napi::Env env = info.Env();
  if (info.Length() < 2) {
    Napi::TypeError::New(env, "mmapFd(fd, size)").ThrowAsJavaScriptException();
    return env.Undefined();
  }

  int fd = info[0].As<Napi::Number>().Int32Value();
  size_t size = static_cast<size_t>(info[1].As<Napi::Number>().Int64Value());

  int dup_fd = ::dup(fd);
  if (dup_fd < 0) {
    Napi::Error::New(env, std::string("dup failed: ") + ::strerror(errno))
        .ThrowAsJavaScriptException();
    return env.Undefined();
  }

  void *ptr = ::mmap(nullptr, size, PROT_READ, MAP_SHARED, dup_fd, 0);
  if (ptr == MAP_FAILED) {
    ::close(dup_fd);
    Napi::Error::New(env, std::string("mmap failed: ") + ::strerror(errno))
        .ThrowAsJavaScriptException();
    return env.Undefined();
  }

  struct MapInfo {
    size_t size;
    int fd;
  };
  MapInfo *hint = new MapInfo{size, dup_fd};

  return Napi::Buffer<uint8_t>::New(
      env, static_cast<uint8_t *>(ptr), size,
      [](Napi::Env, uint8_t *data, MapInfo *info) {
        ::munmap(data, info->size);
        ::close(info->fd);
        delete info;
      },
      hint);
}

Napi::Object ModuleInit(Napi::Env env, Napi::Object exports) {
  exports.Set("init", Napi::Function::New(env, Init));
  exports.Set("importRgba", Napi::Function::New(env, ImportRgba));
  exports.Set("releaseBuffer", Napi::Function::New(env, ReleaseBuffer));
  exports.Set("sendFd", Napi::Function::New(env, SendFd));
  exports.Set("recvFd", Napi::Function::New(env, RecvFd));
  exports.Set("mmapFd", Napi::Function::New(env, MmapFd));
  exports.Set("importDmabufTexture", Napi::Function::New(env, ImportDmabufTexture));
  exports.Set("getTexture", Napi::Function::New(env, GetTexture));
  exports.Set("releaseTexture", Napi::Function::New(env, ReleaseTexture));
  return exports;
}

NODE_API_MODULE(wo_dmabuf, ModuleInit)
