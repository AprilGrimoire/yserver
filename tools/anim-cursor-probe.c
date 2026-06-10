/* anim-cursor-probe — visible smoke test for RENDER CreateAnimCursor.
 *
 * Creates a window whose cursor is a 2-frame animated cursor
 * (solid red / solid blue 24x24, 300ms per frame), then idles.
 * On a server with working animation the cursor visibly blinks
 * red/blue while hovering the window; on static-degeneration
 * servers it stays red.
 *
 * Build: cc -o target/anim-cursor-probe tools/anim-cursor-probe.c -lX11 -lXrender
 * Run:   DISPLAY=:7 ./target/anim-cursor-probe
 */
#include <stdio.h>
#include <X11/Xlib.h>
#include <X11/extensions/Xrender.h>

static Cursor solid_cursor(Display *dpy, Window root,
                           unsigned short r, unsigned short g,
                           unsigned short b) {
    int w = 24, h = 24;
    Pixmap pm = XCreatePixmap(dpy, root, w, h, 32);
    XRenderPictFormat *fmt =
        XRenderFindStandardFormat(dpy, PictStandardARGB32);
    Picture pic = XRenderCreatePicture(dpy, pm, fmt, 0, NULL);
    XRenderColor c = { r, g, b, 0xFFFF };
    XRenderFillRectangle(dpy, PictOpSrc, pic, &c, 0, 0, w, h);
    Cursor cur = XRenderCreateCursor(dpy, pic, 0, 0);
    XRenderFreePicture(dpy, pic);
    XFreePixmap(dpy, pm);
    return cur;
}

int main(void) {
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) { fprintf(stderr, "cannot open display\n"); return 1; }
    Window root = DefaultRootWindow(dpy);

    XAnimCursor frames[2];
    frames[0].cursor = solid_cursor(dpy, root, 0xFFFF, 0, 0);
    frames[0].delay = 300;
    frames[1].cursor = solid_cursor(dpy, root, 0, 0, 0xFFFF);
    frames[1].delay = 300;
    Cursor anim = XRenderCreateAnimCursor(dpy, 2, frames);
    /* libXcursor pattern: constituents freed right after creation —
     * exercises the snapshot/keep-alive lifetime story. */
    XFreeCursor(dpy, frames[0].cursor);
    XFreeCursor(dpy, frames[1].cursor);

    Window win = XCreateSimpleWindow(dpy, root, 50, 50, 400, 300, 1,
                                     0x000000, 0xCCCCCC);
    XStoreName(dpy, win, "anim-cursor-probe");
    XDefineCursor(dpy, win, anim);
    XMapWindow(dpy, win);
    XSync(dpy, False);
    printf("anim cursor 0x%lx defined; hover the window — cursor "
           "should blink red/blue every 300ms. Ctrl-C to exit.\n",
           anim);
    for (;;) {
        XEvent ev;
        XNextEvent(dpy, &ev);
    }
}
