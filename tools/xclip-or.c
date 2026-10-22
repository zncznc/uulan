/* xclip-or.c —— 独立的 X11 剪贴板客户端（真机验收用的「外部应用」）。
 *
 * 只用来当判据：它不是 uulan 的代码，也不和 uulan 共用任何实现，所以它和被控端
 * 之间「能通」才算真的能跟桌面上的其它程序互操作（GTK/Qt 程序走的是同一套
 * ICCCM SelectionRequest/SelectionNotify）。
 *
 * 没有 X11 开发头文件（麒麟源里的 libx11-dev 装不上），所以 Xlib 的符号和事件结构体
 * 都在这里自己声明 —— 只用 1990 年代就定型的核心 ABI。
 *
 * 用法：
 *   ./xclip-or set "文本"      成为 CLIPBOARD 的 owner，一直应答请求（用 timeout 收尾）
 *   ./xclip-or get             请求 CLIPBOARD 的 UTF8_STRING，打印内容
 *   ./xclip-or owner           打印当前 CLIPBOARD 的 owner window id（0 = 没人持有）
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct _XDisplay Display;
typedef unsigned long Window;
typedef unsigned long Atom;
typedef unsigned long Time;
typedef unsigned char KeyCode;
typedef int Bool;

#define True 1
#define False 0
#define CurrentTime 0
#define AnyPropertyType 0
#define PropModeReplace 0
#define Success 0

/* 事件编号（X.h 里定死的值） */
#define PropertyNotify 28
#define SelectionClear 29
#define SelectionRequest 30
#define SelectionNotify 31

extern Display *XOpenDisplay(const char *);
extern int XCloseDisplay(Display *);
extern Window XCreateSimpleWindow(Display *, Window, int, int, unsigned int,
                                  unsigned int, unsigned int, unsigned long,
                                  unsigned long);
extern Window XDefaultRootWindow(Display *);
extern Atom XInternAtom(Display *, const char *, Bool);
extern int XSetSelectionOwner(Display *, Atom, Window, Time);
extern Window XGetSelectionOwner(Display *, Atom);
extern int XConvertSelection(Display *, Atom, Atom, Atom, Window, Time);
extern int XNextEvent(Display *, void *);
extern int XPending(Display *);
extern int XFlush(Display *);
extern int XSync(Display *, Bool);
extern int XFree(void *);
extern int XChangeProperty(Display *, Window, Atom, Atom, int, int,
                           const unsigned char *, int);
extern int XGetWindowProperty(Display *, Window, Atom, long, long, Bool, Atom,
                             Atom *, int *, unsigned long *, unsigned long *,
                             unsigned char **);
extern int XSendEvent(Display *, Window, Bool, long, void *);

/* 事件结构体：XEvent 是 192 字节的联合体，这里用 pad[24] 垫够，避免 XNextEvent 写越界。 */
typedef struct {
    int type;
    unsigned long serial;
    int send_event;
    Display *display;
    Window window;
    Atom atom;
    Time time;
    int state;
} XPropertyLike;

typedef struct {
    int type;
    unsigned long serial;
    int send_event;
    Display *display;
    Window requestor;
    Atom selection;
    Atom target;
    Atom property;
    Time time;
} XSelectionEvent;

typedef struct {
    int type;
    unsigned long serial;
    int send_event;
    Display *display;
    Window owner;
    Window requestor;
    Atom selection;
    Atom target;
    Atom property;
    Time time;
} XSelectionRequestEvent;

typedef union {
    int type;
    XPropertyLike prop;
    XSelectionEvent seln;
    XSelectionRequestEvent selr;
    long pad[24];
} XEv;

static Display *dpy;
static Window win;
static Atom A_CLIPBOARD, A_UTF8, A_STRING, A_TEXT, A_TARGETS, A_ATOM, A_NONE;
static char payload[65536];

static void serve_once(XEv *e) {
    XSelectionRequestEvent *r = &e->selr;
    Atom prop = r->property ? r->property : r->target; /* obsolete clients send None */
    if (r->target == A_TARGETS) {
        Atom list[3] = {A_UTF8, A_STRING, A_TEXT};
        XChangeProperty(dpy, r->requestor, prop, A_ATOM, 32, PropModeReplace,
                        (const unsigned char *)list, 3);
    } else if (r->target == A_UTF8 || r->target == A_TEXT || r->target == A_STRING) {
        XChangeProperty(dpy, r->requestor, prop, r->target, 8, PropModeReplace,
                        (const unsigned char *)payload, (int)strlen(payload));
    } else {
        prop = A_NONE; /* 拒绝：告诉对方「这个 target 我给不了」 */
    }
    XSelectionEvent reply;
    memset(&reply, 0, sizeof(reply));
    reply.type = SelectionNotify;
    reply.display = dpy;
    reply.requestor = r->requestor;
    reply.selection = r->selection;
    reply.target = r->target;
    reply.property = prop;
    reply.time = r->time;
    long mask = 0;
    XSendEvent(dpy, r->requestor, False, mask, &reply);
    XFlush(dpy);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "用法：%s set <文本> | get | owner\n", argv[0]);
        return 2;
    }
    dpy = XOpenDisplay(NULL);
    if (!dpy) {
        fprintf(stderr, "打不开 DISPLAY\n");
        return 1;
    }
    win = XCreateSimpleWindow(dpy, XDefaultRootWindow(dpy), -10, -10, 1, 1, 0, 0, 0);
    A_CLIPBOARD = XInternAtom(dpy, "CLIPBOARD", False);
    A_UTF8 = XInternAtom(dpy, "UTF8_STRING", False);
    A_STRING = XInternAtom(dpy, "STRING", False);
    A_TEXT = XInternAtom(dpy, "TEXT", False);
    A_TARGETS = XInternAtom(dpy, "TARGETS", False);
    A_ATOM = XInternAtom(dpy, "ATOM", False);
    A_NONE = XInternAtom(dpy, "None", False);

    if (strcmp(argv[1], "set") == 0) {
        if (argc < 3) {
            fprintf(stderr, "set 需要文本\n");
            return 2;
        }
        snprintf(payload, sizeof(payload), "%s", argv[2]);
        XSetSelectionOwner(dpy, A_CLIPBOARD, win, CurrentTime);
        XSync(dpy, False);
        if (XGetSelectionOwner(dpy, A_CLIPBOARD) != win) {
            fprintf(stderr, "没能成为 CLIPBOARD 的 owner\n");
            return 1;
        }
        printf("OR-SET-OK len=%zu\n", strlen(payload));
        fflush(stdout);
        for (;;) {
            XEv e;
            XNextEvent(dpy, &e);
            if (e.type == SelectionRequest) {
                serve_once(&e);
            } else if (e.type == SelectionClear) {
                fprintf(stderr, "selection 被抢走了\n");
                return 3;
            }
        }
    }

    if (strcmp(argv[1], "owner") == 0) {
        printf("%lu\n", XGetSelectionOwner(dpy, A_CLIPBOARD));
        return 0;
    }

    if (strcmp(argv[1], "get") == 0) {
        Atom prop = XInternAtom(dpy, "UULAN_OR_TEXT", False);
        XConvertSelection(dpy, A_CLIPBOARD, A_UTF8, prop, win, CurrentTime);
        XFlush(dpy);
        for (;;) {
            XEv e;
            XNextEvent(dpy, &e);
            if (e.type != SelectionNotify || e.seln.selection != A_CLIPBOARD) {
                continue;
            }
            if (e.seln.property == A_NONE || e.seln.property == 0) {
                printf("OR-GET-NONE\n");
                return 4;
            }
            Atom type = 0;
            int fmt = 0;
            unsigned long n = 0, after = 0;
            unsigned char *data = NULL;
            if (XGetWindowProperty(dpy, win, prop, 0, 65536, False, AnyPropertyType,
                                   &type, &fmt, &n, &after, &data) != Success ||
                !data) {
                printf("OR-GET-READFAIL\n");
                return 5;
            }
            printf("OR-GET len=%lu type=%lu\n<<<", n, type);
            fwrite(data, 1, n, stdout);
            printf(">>>\n");
            XFree(data);
            XCloseDisplay(dpy);
            return 0;
        }
    }

    fprintf(stderr, "未知子命令 %s\n", argv[1]);
    return 2;
}