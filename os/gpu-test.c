/* Small bounded GLES2/Lima test on GBM + DRM/KMS. No X server or network.
 *
 * Onscreen presentation is pipelined: the next frame is rendered while the
 * previous page flip is still queued (GBM surface keeps up to 4 buffers; at
 * most two are held by KMS). Atomic KMS is used by default so a smaller render
 * size can be scaled to the display mode by the VOP (--render WxH). --layers N
 * adds N full-screen textured, alpha-blended quads per frame as a UI-like fill
 * load. --verify re-renders a reference frame every second and compares a
 * CRC of a full-width strip, to catch errors at raised GPU clocks.
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <gbm.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <drm_fourcc.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t stop;
static void interrupted(int sig) { (void)sig; stop=1; }
static void die(const char *what) { fprintf(stderr,"FAIL: %s (errno=%d %s EGL=%04x GL=%04x)\n",what,errno,strerror(errno),eglGetError(),glGetError()); exit(1); }
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return t.tv_sec+t.tv_nsec/1e9; }
static double cpu_now(void) { struct timespec t; clock_gettime(CLOCK_PROCESS_CPUTIME_ID,&t); return t.tv_sec+t.tv_nsec/1e9; }
static int clear_only,layers,rgb565;
static double refresh(const drmModeModeInfo *m) { return (double)m->clock*1000/m->htotal/m->vtotal*(m->flags&DRM_MODE_FLAG_INTERLACE?2:1); }

static uint32_t crc32(const unsigned char *p,size_t n) {
    static uint32_t table[256];
    if(!table[1])for(uint32_t i=0;i<256;i++){uint32_t c=i;for(int k=0;k<8;k++)c=c&1?0xedb88320u^(c>>1):c>>1;table[i]=c;}
    uint32_t c=~0u;while(n--)c=table[(c^*p++)&255]^(c>>8);return ~c;
}

static GLuint shader(GLenum kind,const char *src) {
    GLuint s=glCreateShader(kind); glShaderSource(s,1,&src,NULL); glCompileShader(s);
    GLint good=0; glGetShaderiv(s,GL_COMPILE_STATUS,&good);
    if(!good) { char log[2048];glGetShaderInfoLog(s,sizeof(log),NULL,log);fprintf(stderr,"%s\n",log);die("shader compile"); }
    return s;
}
static GLuint program(const char *vs,const char *fs) {
    GLuint p=glCreateProgram();glAttachShader(p,shader(GL_VERTEX_SHADER,vs));glAttachShader(p,shader(GL_FRAGMENT_SHADER,fs));
    glBindAttribLocation(p,0,"pos");glBindAttribLocation(p,1,"color");glLinkProgram(p);
    GLint good=0;glGetProgramiv(p,GL_LINK_STATUS,&good);if(!good)die("program link");
    return p;
}
static GLuint tri_prog,tri_buf,layer_prog,layer_buf,layer_tex;
static GLint rotation,layer_off,layer_alpha;
static void init_scene(void) {
    tri_prog=program("attribute vec2 pos; attribute vec3 color; varying vec3 vcolor; uniform vec2 cs; void main(){vec2 p=vec2(pos.x*cs.x-pos.y*cs.y,pos.x*cs.y+pos.y*cs.x);gl_Position=vec4(p,0.0,1.0);vcolor=color;}",
                     "precision mediump float; varying vec3 vcolor; void main(){gl_FragColor=vec4(vcolor,1.0);}");
    rotation=glGetUniformLocation(tri_prog,"cs");
    const GLfloat vertices[]={-.65f,-.55f,1,0,0, .65f,-.55f,0,1,0, 0,.75f,0,0,1};
    glGenBuffers(1,&tri_buf);glBindBuffer(GL_ARRAY_BUFFER,tri_buf);glBufferData(GL_ARRAY_BUFFER,sizeof(vertices),vertices,GL_STATIC_DRAW);
    if(layers) {
        layer_prog=program("attribute vec2 pos; varying vec2 uv; uniform vec2 off; void main(){uv=(pos*0.5+0.5)*4.0+off;gl_Position=vec4(pos,0.0,1.0);}",
                           "precision mediump float; varying vec2 uv; uniform sampler2D tex; uniform float alpha; void main(){gl_FragColor=vec4(texture2D(tex,uv).rgb,alpha);}");
        layer_off=glGetUniformLocation(layer_prog,"off");layer_alpha=glGetUniformLocation(layer_prog,"alpha");
        glUseProgram(layer_prog);glUniform1i(glGetUniformLocation(layer_prog,"tex"),0);
        const GLfloat quad[]={-1,-1, 1,-1, -1,1, 1,1};
        glGenBuffers(1,&layer_buf);glBindBuffer(GL_ARRAY_BUFFER,layer_buf);glBufferData(GL_ARRAY_BUFFER,sizeof(quad),quad,GL_STATIC_DRAW);
        static unsigned char texels[256*256*4];
        for(int y=0;y<256;y++)for(int x=0;x<256;x++){unsigned char *t=texels+(y*256+x)*4;int c=((x>>5)^(y>>5))&1;
            t[0]=c?200:40;t[1]=(unsigned char)x;t[2]=(unsigned char)y;t[3]=255;}
        glGenTextures(1,&layer_tex);glBindTexture(GL_TEXTURE_2D,layer_tex);
        glTexImage2D(GL_TEXTURE_2D,0,GL_RGBA,256,256,0,GL_RGBA,GL_UNSIGNED_BYTE,texels);
        glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MIN_FILTER,GL_LINEAR);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MAG_FILTER,GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_WRAP_S,GL_REPEAT);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_WRAP_T,GL_REPEAT);
    }
    glDisable(GL_DEPTH_TEST);glDisable(GL_DITHER);glClearColor(.04f,.06f,.10f,1);
    glBlendFunc(GL_SRC_ALPHA,GL_ONE_MINUS_SRC_ALPHA);
}
static void draw(int w,int h,double angle) {
    glViewport(0,0,w,h);
    glClear(GL_COLOR_BUFFER_BIT);
    if(layers) {
        glUseProgram(layer_prog);glBindBuffer(GL_ARRAY_BUFFER,layer_buf);glDisableVertexAttribArray(1);
        glEnableVertexAttribArray(0);glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,0,0);
        glActiveTexture(GL_TEXTURE0);glBindTexture(GL_TEXTURE_2D,layer_tex);glEnable(GL_BLEND);glUniform1f(layer_alpha,.18f);
        for(int i=0;i<layers;i++){glUniform2f(layer_off,(float)(i*.37+angle*.05),(float)(i*.21));glDrawArrays(GL_TRIANGLE_STRIP,0,4);}
        glDisable(GL_BLEND);
    }
    if(!clear_only) {
        glUseProgram(tri_prog);glBindBuffer(GL_ARRAY_BUFFER,tri_buf);
        glEnableVertexAttribArray(0);glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,5*sizeof(GLfloat),0);
        glEnableVertexAttribArray(1);glVertexAttribPointer(1,3,GL_FLOAT,GL_FALSE,5*sizeof(GLfloat),(void*)(2*sizeof(GLfloat)));
        glUniform2f(rotation,cosf(angle),sinf(angle));glDrawArrays(GL_TRIANGLES,0,3);
    }
    if(glGetError()!=GL_NO_ERROR)die("scene draw");
}
static void pixels(int w,int h) {
    unsigned char a[4]={0},b[4]={0};
    glReadPixels(w/2,h/2,1,1,GL_RGBA,GL_UNSIGNED_BYTE,a);
    glReadPixels(1,1,1,1,GL_RGBA,GL_UNSIGNED_BYTE,b);glFinish();
    if(glGetError()!=GL_NO_ERROR)die("pixel readback");
    printf("PIXELS center=%u,%u,%u,%u corner=%u,%u,%u,%u\n",a[0],a[1],a[2],a[3],b[0],b[1],b[2],b[3]);
    if(!layers && (abs((int)b[0]-10)>6 || abs((int)b[1]-15)>6 || abs((int)b[2]-26)>6))
        die("rendered background does not match");
    if(!clear_only && abs((int)a[0]-b[0])+abs((int)a[1]-b[1])+abs((int)a[2]-b[2])<50)
        die("rendered triangle does not differ from background");
    puts("PIXEL_READBACK_PASS");
}
static unsigned char *strip;
static uint32_t strip_crc(int w,int h) {
    glReadPixels(0,h/2-16,w,32,GL_RGBA,GL_UNSIGNED_BYTE,strip);
    if(glGetError()!=GL_NO_ERROR)die("verify readback");
    return crc32(strip,(size_t)w*32*4);
}

struct fb { int fd; uint32_t id; };
static void free_fb(struct gbm_bo *bo,void *p) { (void)bo;struct fb *f=p;drmModeRmFB(f->fd,f->id);free(f); }
static uint32_t framebuffer(int fd,struct gbm_bo *bo) {
    struct fb *f=gbm_bo_get_user_data(bo);if(f)return f->id;
    f=calloc(1,sizeof(*f));f->fd=fd;
    uint32_t handles[4]={gbm_bo_get_handle(bo).u32},pitches[4]={gbm_bo_get_stride(bo)},offsets[4]={0};
    uint64_t modifiers[4]={gbm_bo_get_modifier(bo)};
    uint32_t format=rgb565?DRM_FORMAT_RGB565:DRM_FORMAT_XRGB8888;
    printf("BO_LAYOUT %ux%u pitch=%u modifier=%llx\n",gbm_bo_get_width(bo),gbm_bo_get_height(bo),pitches[0],(unsigned long long)modifiers[0]);
    int result;
    if(modifiers[0]!=DRM_FORMAT_MOD_LINEAR && modifiers[0]!=DRM_FORMAT_MOD_INVALID)
        result=drmModeAddFB2WithModifiers(fd,gbm_bo_get_width(bo),gbm_bo_get_height(bo),format,handles,pitches,offsets,modifiers,&f->id,DRM_MODE_FB_MODIFIERS);
    else result=drmModeAddFB2(fd,gbm_bo_get_width(bo),gbm_bo_get_height(bo),format,handles,pitches,offsets,&f->id,0);
    if(result)die("DRM add framebuffer");
    gbm_bo_set_user_data(bo,f,free_fb);return f->id;
}
static int flips_done;
static void flip(int fd,unsigned frame,unsigned sec,unsigned usec,void *p) { (void)fd;(void)frame;(void)sec;(void)usec;*(int*)p=0;flips_done++; }
static int wait_flip(int fd,int *pending) {
    drmEventContext ev={.version=2,.page_flip_handler=flip};
    double deadline=now()+3;
    while(*pending && now()<deadline) {
        struct pollfd p={.fd=fd,.events=POLLIN};int n=poll(&p,1,200);
        if(n>0 && (p.revents&POLLIN)) { if(drmHandleEvent(fd,&ev))return -1; }
        else if(n<0 && errno!=EINTR)return -1;
    }
    return *pending?-1:0;
}

/* Atomic KMS state for one connector -> CRTC -> primary plane. */
struct kms { int fd; uint32_t conn,crtc,plane,blob;
    uint32_t c_crtc,r_mode,r_active,p_fb,p_crtc,p_sx,p_sy,p_sw,p_sh,p_cx,p_cy,p_cw,p_ch; };
static uint32_t prop(int fd,uint32_t obj,uint32_t type,const char *name,uint64_t *value) {
    drmModeObjectProperties *p=drmModeObjectGetProperties(fd,obj,type);uint32_t id=0;
    for(uint32_t i=0;p && i<p->count_props;i++) {
        drmModePropertyRes *r=drmModeGetProperty(fd,p->props[i]);
        if(r && !strcmp(r->name,name)) { id=r->prop_id;if(value)*value=p->prop_values[i]; }
        drmModeFreeProperty(r);
    }
    drmModeFreeObjectProperties(p);
    if(!id) { fprintf(stderr,"missing KMS property %s\n",name);die("atomic property"); }
    return id;
}
static void kms_init(struct kms *k,int fd,drmModeRes *res,uint32_t conn,uint32_t crtc) {
    memset(k,0,sizeof(*k));k->fd=fd;k->conn=conn;k->crtc=crtc;
    if(drmSetClientCap(fd,DRM_CLIENT_CAP_UNIVERSAL_PLANES,1)||drmSetClientCap(fd,DRM_CLIENT_CAP_ATOMIC,1))die("atomic KMS client caps");
    int index=-1;for(int i=0;i<res->count_crtcs;i++)if(res->crtcs[i]==crtc)index=i;
    drmModePlaneRes *planes=drmModeGetPlaneResources(fd);if(!planes||index<0)die("KMS planes");
    for(uint32_t i=0;i<planes->count_planes && !k->plane;i++) {
        drmModePlane *pl=drmModeGetPlane(fd,planes->planes[i]);uint64_t type=0;
        if(pl && (pl->possible_crtcs&(1u<<index))) {
            prop(fd,pl->plane_id,DRM_MODE_OBJECT_PLANE,"type",&type);
            if(type==DRM_PLANE_TYPE_PRIMARY)k->plane=pl->plane_id;
        }
        drmModeFreePlane(pl);
    }
    drmModeFreePlaneResources(planes);if(!k->plane)die("primary plane");
    k->c_crtc=prop(fd,conn,DRM_MODE_OBJECT_CONNECTOR,"CRTC_ID",NULL);
    k->r_mode=prop(fd,crtc,DRM_MODE_OBJECT_CRTC,"MODE_ID",NULL);k->r_active=prop(fd,crtc,DRM_MODE_OBJECT_CRTC,"ACTIVE",NULL);
    uint32_t pl=k->plane,t=DRM_MODE_OBJECT_PLANE;
    k->p_fb=prop(fd,pl,t,"FB_ID",NULL);k->p_crtc=prop(fd,pl,t,"CRTC_ID",NULL);
    k->p_sx=prop(fd,pl,t,"SRC_X",NULL);k->p_sy=prop(fd,pl,t,"SRC_Y",NULL);k->p_sw=prop(fd,pl,t,"SRC_W",NULL);k->p_sh=prop(fd,pl,t,"SRC_H",NULL);
    k->p_cx=prop(fd,pl,t,"CRTC_X",NULL);k->p_cy=prop(fd,pl,t,"CRTC_Y",NULL);k->p_cw=prop(fd,pl,t,"CRTC_W",NULL);k->p_ch=prop(fd,pl,t,"CRTC_H",NULL);
    printf("ATOMIC_KMS plane=%u crtc=%u connector=%u\n",k->plane,crtc,conn);
}
static int kms_modeset(struct kms *k,drmModeModeInfo *mode,uint32_t fb,int rw,int rh) {
    if(drmModeCreatePropertyBlob(k->fd,mode,sizeof(*mode),&k->blob))return -1;
    drmModeAtomicReq *q=drmModeAtomicAlloc();
    drmModeAtomicAddProperty(q,k->conn,k->c_crtc,k->crtc);
    drmModeAtomicAddProperty(q,k->crtc,k->r_mode,k->blob);drmModeAtomicAddProperty(q,k->crtc,k->r_active,1);
    drmModeAtomicAddProperty(q,k->plane,k->p_fb,fb);drmModeAtomicAddProperty(q,k->plane,k->p_crtc,k->crtc);
    drmModeAtomicAddProperty(q,k->plane,k->p_sx,0);drmModeAtomicAddProperty(q,k->plane,k->p_sy,0);
    drmModeAtomicAddProperty(q,k->plane,k->p_sw,(uint64_t)rw<<16);drmModeAtomicAddProperty(q,k->plane,k->p_sh,(uint64_t)rh<<16);
    drmModeAtomicAddProperty(q,k->plane,k->p_cx,0);drmModeAtomicAddProperty(q,k->plane,k->p_cy,0);
    drmModeAtomicAddProperty(q,k->plane,k->p_cw,mode->hdisplay);drmModeAtomicAddProperty(q,k->plane,k->p_ch,mode->vdisplay);
    int r=drmModeAtomicCommit(k->fd,q,DRM_MODE_ATOMIC_ALLOW_MODESET,NULL);drmModeAtomicFree(q);return r;
}
static int kms_flip(struct kms *k,uint32_t fb,int *pending) {
    drmModeAtomicReq *q=drmModeAtomicAlloc();
    drmModeAtomicAddProperty(q,k->plane,k->p_fb,fb);
    int r=drmModeAtomicCommit(k->fd,q,DRM_MODE_PAGE_FLIP_EVENT|DRM_MODE_ATOMIC_NONBLOCK,pending);drmModeAtomicFree(q);return r;
}

int main(int argc,char **argv) {
    int w=1280,h=720,hz=60,rw=0,rh=0,seconds=12,fps=30,offscreen=0,list=0,force=0;
    int profile=0,finish_each=0,native_buffers=0,render_node=0,no_egl_vsync=0,legacy=0,serial=0,verify=0,inflight=0;
    for(int i=1;i<argc;i++) {
        if(!strcmp(argv[i],"--offscreen"))offscreen=1;
        else if(!strcmp(argv[i],"--list"))list=1;
        else if(!strcmp(argv[i],"--force-mode"))force=1;
        else if(!strcmp(argv[i],"--profile"))profile=1;
        else if(!strcmp(argv[i],"--finish"))finish_each=1;
        else if(!strcmp(argv[i],"--native-buffers"))native_buffers=1;
        else if(!strcmp(argv[i],"--render-node"))render_node=1;
        else if(!strcmp(argv[i],"--no-egl-vsync"))no_egl_vsync=1;
        else if(!strcmp(argv[i],"--clear-only"))clear_only=1;
        else if(!strcmp(argv[i],"--legacy"))legacy=1;
        else if(!strcmp(argv[i],"--serial"))serial=1;
        else if(!strcmp(argv[i],"--verify"))verify=1;
        else if(!strcmp(argv[i],"--rgb565"))rgb565=1;
        else if(!strcmp(argv[i],"--mode") && i+1<argc) { if(sscanf(argv[++i],"%dx%d@%d",&w,&h,&hz)!=3)return 2; }
        else if(!strcmp(argv[i],"--render") && i+1<argc) { if(sscanf(argv[++i],"%dx%d",&rw,&rh)!=2)return 2; }
        else if(!strcmp(argv[i],"--seconds") && i+1<argc)seconds=atoi(argv[++i]);
        else if(!strcmp(argv[i],"--fps") && i+1<argc)fps=atoi(argv[++i]);
        else if(!strcmp(argv[i],"--layers") && i+1<argc)layers=atoi(argv[++i]);
        else if(!strcmp(argv[i],"--inflight") && i+1<argc)inflight=atoi(argv[++i]);
        else { fprintf(stderr,"Usage: gpu-test [--list] [--offscreen] [--mode WxH@Hz] [--render WxH] [--seconds 1..60] [--fps 0..240]\n"
                       "  [--layers 0..32] [--rgb565] [--verify] [--inflight 0..3 (offscreen)] [--serial] [--legacy]\n"
                       "  [--force-mode] [--profile] [--finish] [--native-buffers] [--render-node] [--no-egl-vsync] [--clear-only]\n");return 2; }
    }
    if(!rw) { rw=w;rh=h; }
    if(w<64||h<64||w>4096||h>4096||rw<64||rh<64||rw>4096||rh>4096||seconds<1||seconds>60||fps<0||fps>240||
       layers<0||layers>32||inflight<0||inflight>3||(render_node&&!offscreen)||(legacy&&(rw!=w||rh!=h)))return 2;
    setvbuf(stdout,NULL,_IOLBF,0);signal(SIGINT,interrupted);signal(SIGTERM,interrupted);
    setenv("MESA_SHADER_CACHE_DISABLE","true",1);setenv("EGL_LOG_LEVEL","debug",0);
    /* Default Lima PP-stream cache is far too small for 4K targets: the PP
     * stream is regenerated on the CPU every frame (measured 3 -> 27 fps). */
    setenv("LIMA_PLB_PP_STREAM_CACHE_SIZE","4194304",0);
    printf("LIMA_ENV pp_stream_cache=%s ctx_num_plb=%s\n",getenv("LIMA_PLB_PP_STREAM_CACHE_SIZE"),getenv("LIMA_CTX_NUM_PLB")?getenv("LIMA_CTX_NUM_PLB"):"default");
    int fd=open("/dev/dri/card0",O_RDWR|O_CLOEXEC);if(fd<0)die("open KMS card0");
    drmVersionPtr ver=drmGetVersion(fd);if(!ver || strcmp(ver->name,"rockchip"))die("card0 is not rockchip");
    printf("KMS_DRIVER=%s %d.%d.%d\n",ver->name,ver->version_major,ver->version_minor,ver->version_patchlevel);drmFreeVersion(ver);
    drmModeRes *res=drmModeGetResources(fd);if(!res)die("KMS resources");
    drmModeConnector *conn=NULL;
    for(int i=0;i<res->count_connectors;i++) {
        drmModeConnector *c=drmModeGetConnector(fd,res->connectors[i]);
        if(c && c->connection==DRM_MODE_CONNECTED) { conn=c;break; }
        drmModeFreeConnector(c);
    }
    if(!conn && !offscreen)die("no connected KMS display");
    drmModeModeInfo selected={0};int found=0;
    if(conn)for(int i=0;i<conn->count_modes;i++) {
        drmModeModeInfo *m=&conn->modes[i];double r=refresh(m);
        if(list)printf("MODE %dx%d@%.3f clock=%u flags=%x type=%x\n",m->hdisplay,m->vdisplay,r,m->clock,m->flags,m->type);
        if(!found && m->hdisplay==w && m->vdisplay==h && fabs(r-hz)<.8 && !(m->flags&DRM_MODE_FLAG_INTERLACE)) { selected=*m;found=1; }
    }
    if(list)return 0;
    if(!offscreen && !found && force && w==3840 && h==2160 && (hz==30||hz==60)) {
        selected=(drmModeModeInfo){.clock=hz==30?297000:594000,.hdisplay=3840,.hsync_start=4016,.hsync_end=4104,.htotal=4400,
            .vdisplay=2160,.vsync_start=2168,.vsync_end=2178,.vtotal=2250,.vrefresh=hz,
            .flags=DRM_MODE_FLAG_PHSYNC|DRM_MODE_FLAG_PVSYNC,.type=DRM_MODE_TYPE_USERDEF};
        snprintf(selected.name,sizeof(selected.name),"3840x2160");found=1;puts("FORCED_CEA_MODE: kernel may reject this timing");
    }
    if(!offscreen && !found) { fprintf(stderr,"REQUESTED_MODE_UNAVAILABLE %dx%d@%d\n",w,h,hz);return 3; }
    if(found)printf("MODE_SELECTED %dx%d@%.3f clock=%u\n",selected.hdisplay,selected.vdisplay,refresh(&selected),selected.clock);
    uint32_t crtc_id=0;
    if(conn) {
        drmModeEncoder *enc=drmModeGetEncoder(fd,conn->encoder_id);
        if(enc) { crtc_id=enc->crtc_id;drmModeFreeEncoder(enc); }
        if(!crtc_id && res->count_crtcs)crtc_id=res->crtcs[0];
    }
    drmModeCrtc *saved=crtc_id?drmModeGetCrtc(fd,crtc_id):NULL;
    struct kms k;if(!offscreen && !legacy)kms_init(&k,fd,res,conn->connector_id,crtc_id);
    int renderfd=render_node?open("/dev/dri/renderD128",O_RDWR|O_CLOEXEC):fd;
    if(renderfd<0)die("render node");
    struct gbm_device *gbm=gbm_create_device(renderfd);if(!gbm)die("GBM device");
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform=(void*)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if(!get_platform)die("EGL platform display extension");
    EGLDisplay display=get_platform(EGL_PLATFORM_GBM_KHR,gbm,NULL);EGLint major,minor;
    if(display==EGL_NO_DISPLAY||!eglInitialize(display,&major,&minor))die("EGL initialize GBM");
    printf("EGL=%d.%d VENDOR=%s VERSION=%s\n",major,minor,eglQueryString(display,EGL_VENDOR),eglQueryString(display,EGL_VERSION));
    uint32_t visual=rgb565?GBM_FORMAT_RGB565:GBM_FORMAT_XRGB8888;
    EGLint attrs[]={EGL_SURFACE_TYPE,EGL_WINDOW_BIT,EGL_RENDERABLE_TYPE,EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE,rgb565?5:8,EGL_GREEN_SIZE,rgb565?6:8,EGL_BLUE_SIZE,rgb565?5:8,EGL_ALPHA_SIZE,0,EGL_DEPTH_SIZE,0,EGL_STENCIL_SIZE,0,EGL_NONE};
    EGLConfig configs[64],cfg=NULL;EGLint count;
    if(!eglChooseConfig(display,attrs,configs,64,&count))die("EGL config");
    for(int i=0;i<count;i++) { EGLint format;eglGetConfigAttrib(display,configs[i],EGL_NATIVE_VISUAL_ID,&format);if((uint32_t)format==visual){cfg=configs[i];break;} }
    if(!cfg)die(rgb565?"RGB565 EGL config":"XRGB8888 EGL config");
    eglBindAPI(EGL_OPENGL_ES_API);EGLint ca[]={EGL_CONTEXT_CLIENT_VERSION,2,EGL_NONE};
    EGLContext ctx=eglCreateContext(display,cfg,EGL_NO_CONTEXT,ca);if(ctx==EGL_NO_CONTEXT)die("GLES2 context");
    struct gbm_surface *surface=NULL;EGLSurface window=EGL_NO_SURFACE;
    if(!offscreen) {
        uint32_t usage=GBM_BO_USE_SCANOUT|GBM_BO_USE_RENDERING;
        if(!native_buffers)usage|=GBM_BO_USE_LINEAR;
        surface=gbm_surface_create(gbm,rw,rh,visual,usage);
        if(!surface)die("GBM scanout surface");
        window=eglCreateWindowSurface(display,cfg,(EGLNativeWindowType)surface,NULL);
        if(window==EGL_NO_SURFACE)die("EGL window surface");
    }
    if(!eglMakeCurrent(display,window,window,ctx))die("EGL make current");
    if(no_egl_vsync && !eglSwapInterval(display,0))die("EGL swap interval");
    const char *renderer=(const char*)glGetString(GL_RENDERER);
    printf("GL_VENDOR=%s\nGL_RENDERER=%s\nGL_VERSION=%s\n",glGetString(GL_VENDOR),renderer,glGetString(GL_VERSION));
    if(!renderer || (!strstr(renderer,"Mali400")&&!strstr(renderer,"Mali-400")))die("hardware Mali400 renderer required; software fallback rejected");
    GLint maxtex,maxrb,viewport[2];glGetIntegerv(GL_MAX_TEXTURE_SIZE,&maxtex);glGetIntegerv(GL_MAX_RENDERBUFFER_SIZE,&maxrb);glGetIntegerv(GL_MAX_VIEWPORT_DIMS,viewport);
    printf("LIMITS texture=%d renderbuffer=%d viewport=%dx%d\n",maxtex,maxrb,viewport[0],viewport[1]);
    if(rw>maxtex||rh>maxtex||rw>maxrb||rh>maxrb||rw>viewport[0]||rh>viewport[1])die("requested render dimensions exceed GPU limits");
    GLuint tex=0,fbo=0,rb=0;
    if(offscreen) {
        glGenFramebuffers(1,&fbo);glBindFramebuffer(GL_FRAMEBUFFER,fbo);
        if(rgb565) {
            glGenRenderbuffers(1,&rb);glBindRenderbuffer(GL_RENDERBUFFER,rb);glRenderbufferStorage(GL_RENDERBUFFER,GL_RGB565,rw,rh);
            glFramebufferRenderbuffer(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_RENDERBUFFER,rb);
        } else {
            glGenTextures(1,&tex);glBindTexture(GL_TEXTURE_2D,tex);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MIN_FILTER,GL_NEAREST);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MAG_FILTER,GL_NEAREST);
            glTexImage2D(GL_TEXTURE_2D,0,GL_RGBA,rw,rh,0,GL_RGBA,GL_UNSIGNED_BYTE,NULL);
            glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,tex,0);
        }
        if(glCheckFramebufferStatus(GL_FRAMEBUFFER)!=GL_FRAMEBUFFER_COMPLETE)die("offscreen FBO");
    }
    /* Optional fence ring keeps up to `inflight` offscreen frames queued. */
    PFNEGLCREATESYNCKHRPROC create_sync=NULL;PFNEGLCLIENTWAITSYNCKHRPROC wait_sync=NULL;PFNEGLDESTROYSYNCKHRPROC destroy_sync=NULL;
    EGLSyncKHR ring[4]={0};int ring_n=0;
    if(offscreen && inflight) {
        const char *ext=eglQueryString(display,EGL_EXTENSIONS);
        if(ext && strstr(ext,"EGL_KHR_fence_sync")) {
            create_sync=(void*)eglGetProcAddress("eglCreateSyncKHR");wait_sync=(void*)eglGetProcAddress("eglClientWaitSyncKHR");destroy_sync=(void*)eglGetProcAddress("eglDestroySyncKHR");
        }
        if(!create_sync||!wait_sync||!destroy_sync) { puts("INFLIGHT_UNAVAILABLE no EGL_KHR_fence_sync; using glFinish");inflight=0; }
    }
    init_scene();draw(rw,rh,0);pixels(rw,rh);
    uint32_t ref=0;int checks=0,mismatches=0;
    if(verify) { strip=malloc((size_t)rw*32*4);if(!strip)die("verify buffer");ref=strip_crc(rw,rh);printf("VERIFY_REFERENCE crc=%08x strip=%dx32\n",ref,rw); }
    printf("RENDER_START render=%dx%d scanout=%s %s layers=%d format=%s target_fps=%d seconds=%d inflight=%d present=%s\n",rw,rh,
           offscreen?"unchanged":selected.name,offscreen?"":"scaled-by-vop-if-smaller",layers,rgb565?"RGB565":"XRGB8888",fps,seconds,inflight,
           offscreen?"none":legacy?(serial?"legacy-serial":"legacy-pipelined"):(serial?"atomic-serial":"atomic-pipelined"));
    struct gbm_bo *shown=NULL,*queued=NULL;int modeset=0,frames=0,failed=0,pending=0;
    double start=now(),deadline=start+seconds,cpu_start=cpu_now(),next_verify=start+1;
    double td=0,tf=0,ts=0,tl=0,tk=0,tw=0,tn=0;
    while(!stop && now()<deadline) {
        double begin=now(),mark=begin;
        int check=verify && begin>=next_verify;
        if(check) {
            /* Reference frame (angle 0) for the CRC, then the real frame on
             * top, so onscreen runs never present the reference pose. */
            draw(rw,rh,0);
            uint32_t c=strip_crc(rw,rh);checks++;next_verify=begin+1;
            if(c!=ref) { mismatches++;printf("VERIFY_MISMATCH frame=%d crc=%08x expected=%08x\n",frames,c,ref); }
        }
        draw(rw,rh,(begin-start)*.7);
        td+=now()-mark;
        if(offscreen) {
            mark=now();
            if(!inflight || finish_each) glFinish();
            else {
                glFlush();
                if(ring_n==inflight) { wait_sync(display,ring[0],EGL_SYNC_FLUSH_COMMANDS_BIT_KHR,EGL_FOREVER_KHR);destroy_sync(display,ring[0]);
                    memmove(ring,ring+1,sizeof(ring[0])*(inflight-1));ring_n--; }
                ring[ring_n++]=create_sync(display,EGL_SYNC_FENCE_KHR,NULL);
            }
            tf+=now()-mark;
        } else {
            if(finish_each) { mark=now();glFinish();tf+=now()-mark; }
            mark=now();
            if(!eglSwapBuffers(display,window)) { failed=1;break; }
            ts+=now()-mark;mark=now();
            struct gbm_bo *bo=gbm_surface_lock_front_buffer(surface);if(!bo){failed=1;break;}
            tl+=now()-mark;mark=now();
            /* Wait for the previously queued flip only now, after the next
             * frame has already been rendered and submitted. */
            if(pending) {
                if(wait_flip(fd,&pending)) { fprintf(stderr,"PAGE_FLIP_TIMEOUT\n");failed=1;break; }
                if(shown)gbm_surface_release_buffer(surface,shown);
                shown=queued;queued=NULL;
            }
            tw+=now()-mark;mark=now();
            uint32_t fb=framebuffer(fd,bo);
            if(!modeset) {
                int r=legacy?drmModeSetCrtc(fd,crtc_id,fb,0,0,&conn->connector_id,1,&selected):kms_modeset(&k,&selected,fb,rw,rh);
                if(r) {
                    fprintf(stderr,"KMS_MODESET_FAILED size=%dx%d@%.3f render=%dx%d errno=%d %s\n",w,h,refresh(&selected),rw,rh,errno,strerror(errno));
                    gbm_surface_release_buffer(surface,bo);failed=1;break;
                }
                modeset=1;shown=bo;flips_done++;
                printf("KMS_MODESET_PASS %dx%d@%.3f render=%dx%d\n",w,h,refresh(&selected),rw,rh);
                tk+=now()-mark;
            } else {
                int r=legacy?drmModePageFlip(fd,crtc_id,fb,DRM_MODE_PAGE_FLIP_EVENT,&pending):kms_flip(&k,fb,&pending);
                if(r) { fprintf(stderr,"PAGE_FLIP_FAILED errno=%d %s\n",errno,strerror(errno));gbm_surface_release_buffer(surface,bo);failed=1;break; }
                pending=1;queued=bo;
                tk+=now()-mark;
                if(serial) {
                    mark=now();
                    if(wait_flip(fd,&pending)) { fprintf(stderr,"PAGE_FLIP_TIMEOUT\n");failed=1;break; }
                    if(shown)gbm_surface_release_buffer(surface,shown);
                    shown=queued;queued=NULL;tw+=now()-mark;
                }
            }
        }
        frames++;
        double left=fps?1.0/fps-(now()-begin):0;
        if(left>0) { mark=now();struct timespec nap={.tv_sec=0,.tv_nsec=(long)(left*1e9)};nanosleep(&nap,NULL);tn+=now()-mark; }
    }
    if(offscreen) { glFinish();while(ring_n)destroy_sync(display,ring[--ring_n]); }
    if(pending && !wait_flip(fd,&pending)) { if(shown)gbm_surface_release_buffer(surface,shown);shown=queued;queued=NULL; }
    double elapsed=now()-start;
    int shown_frames=offscreen?frames:flips_done;
    printf("RENDER_RESULT frames=%d presented=%d elapsed=%.3f fps=%.2f render=%dx%d onscreen=%d status=%s\n",frames,shown_frames,elapsed,shown_frames/elapsed,rw,rh,!offscreen,failed||mismatches?"FAIL":"PASS");
    if(verify)printf("VERIFY checks=%d mismatches=%d %s\n",checks,mismatches,mismatches?"VERIFY_FAIL":"VERIFY_PASS");
    if(profile) {
        double scale=frames?1000.0/frames:0;
        printf("PROFILE cpu_seconds=%.3f cpu_pct=%.1f ms_per_frame draw=%.3f finish=%.3f swap=%.3f lock=%.3f kms=%.3f flipwait=%.3f sleep=%.3f\n",cpu_now()-cpu_start,(cpu_now()-cpu_start)/elapsed*100,td*scale,tf*scale,ts*scale,tl*scale,tk*scale,tw*scale,tn*scale);
    }
    if(modeset && saved) {
        int result=saved->mode_valid?drmModeSetCrtc(fd,saved->crtc_id,saved->buffer_id,saved->x,saved->y,&conn->connector_id,1,&saved->mode):drmModeSetCrtc(fd,crtc_id,0,0,0,NULL,0,NULL);
        printf("RESTORE_CRTC=%d\n",result);
    }
    if(shown)gbm_surface_release_buffer(surface,shown);
    if(queued)gbm_surface_release_buffer(surface,queued);
    if(fbo)glDeleteFramebuffers(1,&fbo);
    if(tex)glDeleteTextures(1,&tex);
    if(rb)glDeleteRenderbuffers(1,&rb);
    eglMakeCurrent(display,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);
    if(window!=EGL_NO_SURFACE)eglDestroySurface(display,window);
    eglDestroyContext(display,ctx);eglTerminate(display);
    if(surface)gbm_surface_destroy(surface);
    gbm_device_destroy(gbm);
    drmModeFreeCrtc(saved);drmModeFreeConnector(conn);drmModeFreeResources(res);close(fd);
    if(render_node)close(renderfd);
    return failed||mismatches?1:0;
}
