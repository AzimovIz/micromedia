# Сборка портативной libmpv.so

Цель: получить **одну** `libmpv.so`, которая зависит только от glibc и системной
графики/звука — без cdio, mujs, lua и прочих необязательных библиотек. Тогда
приложение можно носить на флешке: `appdata/libs/libmpv.so` подхватится
загрузчиком в [`open_libmpv()`](../src/mpv.rs), и никаких «положи ещё одну .so»
не потребуется.

## Почему дистрибутивная libmpv не годится

Сборки из репозиториев (Arch, Fedora, …) включают максимум необязательных
фич. Каждая фича — это новая запись `DT_NEEDED`, то есть ещё одна библиотека,
которая должна найтись на чужой машине. Хуже того, mujs в некоторых
дистрибутивах ставится **без SONAME**, и в libmpv попадает абсолютный путь:

```
$ readelf -d libmpv.so | grep NEEDED
 0x0001 (NEEDED)  Shared library: [/usr/lib/libmujs.so]   # ← абсолютный путь!
```

Динамический загрузчик, увидев в записи слэш, идёт строго по этому пути и
**не смотрит** ни в `LD_LIBRARY_PATH`, ни в `RPATH`. Такую библиотеку нельзя
сделать переносимой, не патча её бинарно.

## Где собирать

**В контейнере со старым glibc.** Библиотека, собранная на glibc 2.39,
не запустится на системе с 2.31 — а наоборот работает. Берём Debian 11
(glibc 2.31) как разумный компромисс:

```bash
podman run --rm -it -v "$PWD:/out" debian:11 bash
```

Внутри контейнера — всё, что ниже.

## Зависимости сборки

```bash
apt update && apt install -y \
  git build-essential ninja-build pkg-config python3 python3-pip \
  autoconf automake libtool \
  nasm yasm \
  libfreetype6-dev libfribidi-dev libharfbuzz-dev libfontconfig-dev \
  libasound2-dev \
  zlib1g-dev

# meson из apt в Debian 11 — 0.56, а libplacebo требует >= 1.3. Ставим свежий.
pip3 install --upgrade meson
meson --version   # должно быть >= 1.3
```

Что здесь зачем:

- **X11/EGL здесь намеренно нет.** Поддержку OpenGL для render API даёт опция
  `-Dplain-gl=enabled` (см. ниже): GL-функции mpv берёт через `get_proc_address`
  из контекста Slint и сам к графическим библиотекам не линкуется. Меньше
  зависимостей — меньше поводов сломаться на чужой машине.
- **autoconf/automake/libtool** — libass собирается через autotools (`autogen.sh`),
  без них mpv-build падает на `autoreconf: not found`.
- **freetype/fribidi/harfbuzz/fontconfig** — нужны libass (субтитры); без
  fontconfig его configure падает с «No system font provider!». Они останутся
  динамическими зависимостями итоговой `libmpv.so`, но это нормально: на любой
  desktop-системе с шрифтами они есть. (Тот же `libfontconfig-dev` нужен и для
  сборки самого приложения — см. README.)
- **ALSA** — единственный включаемый звуковой выход. PulseAudio/PipeWire
  намеренно выключаем: они добавили бы `libpulse`/`libpipewire` в зависимости,
  а на машине без них libmpv просто не загрузится. ALSA есть везде и
  прозрачно работает поверх Pulse/PipeWire.

## Сборка

```bash
git clone https://github.com/mpv-player/mpv-build.git
cd mpv-build
./use-ffmpeg-release        # стабильный релиз ffmpeg вместо master

# mpv: только библиотека, всё необязательное — вон
printf '%s\n' \
  -Dlibmpv=true \
  -Dcplayer=false \
  -Dgpl=true \
  -Dplain-gl=enabled \
  -Dgl=enabled \
  -Dalsa=enabled \
  -Djpeg=disabled \
  -Dzlib=enabled \
  -Diconv=enabled \
  -Dcdda=disabled \
  -Ddvdnav=disabled \
  -Ddvbin=disabled \
  -Djavascript=disabled \
  -Dlua=disabled \
  -Dcplugins=disabled \
  -Dvapoursynth=disabled \
  -Duchardet=disabled \
  -Drubberband=disabled \
  -Dlibbluray=disabled \
  -Dlibarchive=disabled \
  -Dlibavdevice=disabled \
  -Dlibcurl=disabled \
  -Dlcms2=disabled \
  -Dcaca=disabled \
  -Dsixel=disabled \
  -Dsdl2-audio=disabled \
  -Dsdl2-video=disabled \
  -Dsdl2-gamepad=disabled \
  -Dpulse=disabled \
  -Dpipewire=disabled \
  -Djack=disabled \
  -Dopenal=disabled \
  -Dsndio=disabled \
  -Doss-audio=disabled \
  -Dx11=disabled \
  -Dxv=disabled \
  -Dwayland=disabled \
  -Ddrm=disabled \
  -Dgbm=disabled \
  -Degl=disabled \
  -Degl-x11=disabled \
  -Degl-drm=disabled \
  -Degl-wayland=disabled \
  -Dgl-x11=disabled \
  -Dvulkan=disabled \
  -Dshaderc=disabled \
  -Dspirv-cross=disabled \
  -Dvaapi=disabled \
  -Dvdpau=disabled \
  -Dcuda-hwaccel=disabled \
  -Dcuda-interop=disabled \
  >> mpv_options

# ffmpeg: не подхватывать системные библиотеки автоматически
printf '%s\n' \
  --disable-autodetect \
  --disable-programs \
  --disable-doc \
  --enable-zlib \
  --enable-gpl \
  --enable-version3 \
  >> ffmpeg_options

./rebuild -j"$(nproc)"
```

Про ключевые опции mpv:

- **`-Dplain-gl=enabled`** — «OpenGL без платформенного кода». Ровно то, что нужно
  render API: GL-функции mpv получает через наш `get_proc_address` из контекста
  Slint, а сам ни к libGL, ни к libEGL не линкуется. Поэтому мы можем выключить
  `x11`, `wayland`, `drm`, `egl-*` — своих окон mpv не открывает (`vo=libmpv`),
  и лишних зависимостей не тянет.
- **`-Dcuda-hwaccel=disabled -Dcuda-interop=disabled`** — заодно навсегда убирает
  то самое `Cannot load libcuda.so.1`: mpv просто не будет пробовать CUDA.
- **`-Dvaapi=disabled -Dvdpau=disabled`** — иначе появятся зависимости `libva` /
  `libvdpau`. Аппаратный декод у нас всё равно выключен (`hwdec=no` в
  [src/mpv.rs](../src/mpv.rs)) из-за артефактов интеропа.
- **`-Djpeg=disabled`** — в Debian 11 нет pkg-config файла для libjpeg, да и
  незачем: снимок кадра (`screenshot-to-file` в [src/main.rs](../src/main.rs))
  сохраняем во временный **PNG** (mpv умеет его через zlib), а в JPEG-миниатюру
  его всё равно перекодирует крейт `image`. Требует правки расширения `.cap.jpg`
  → `.cap.png` в коде.
- Если meson ругнётся `Unknown option: "…"` — имена опций между версиями mpv
  меняются, актуальный список лежит в `mpv/meson.options` (раньше файл назывался
  `meson_options.txt`).

`--disable-autodetect` — важная строка: без неё ffmpeg молча прилинкует всё,
что найдёт в системе (dav1d, vpx, opus…), и каждая такая библиотека станет
внешней зависимостью итоговой `libmpv.so`. Цена: AV1 декодируется встроенным
декодером ffmpeg, он заметно медленнее dav1d. Если AV1 важен — добавьте
`libdav1d-dev` в зависимости и `--enable-libdav1d`, но тогда `libdav1d.so`
придётся класть рядом (см. «Если зависимость всё же осталась»).

`mpv-build` сам включает PIC для ffmpeg, libass и libplacebo, когда собирается
libmpv, — они влинковываются статически внутрь `.so`.

Результат: `mpv/build/libmpv.so.2` (символическая ссылка на
`libmpv.so.2.<...>`).

## Проверка — это обязательный шаг

```bash
readelf -d mpv/build/libmpv.so.2 | grep NEEDED
```

Ожидаем увидеть **только** системное: `libc`, `libm`, `libdl`, `libpthread`,
`libGL`/`libEGL`, `libX11` (+`libXext`, `libxcb`…), `libasound`, `libz`,
`libstdc++`. Ни одной записи с абсолютным путём (`/usr/lib/...`) и ничего вроде
`libmujs`, `libcdio`, `libavcodec` — ffmpeg должен быть внутри, а не снаружи.

```bash
ldd mpv/build/libmpv.so.2       # то же самое, но покажет, что не резолвится
```

## Установка в проект

```bash
cp mpv/build/libmpv.so.2 /out/appdata/libs/libmpv.so
```

Загрузчик ищет библиотеку в `appdata/libs/` и `libs/` рядом с бинарником
(см. `find_libmpv()` в [src/mpv.rs](../src/mpv.rs)). Учтите: **сначала**
пробуется системная libmpv, и только потом бандл. Чтобы проверить именно свою
сборку, форсируйте её:

```bash
MICROMEDIA_MPV=./appdata/libs/libmpv.so RUST_LOG=info ./micromedia
```

В логе должна появиться строка `Использую MICROMEDIA_MPV: …`.

## Если зависимость всё же осталась

Скажем, вы включили dav1d и получили `NEEDED libdav1d.so.7`. Тогда кладём её
рядом и заставляем libmpv искать в своей же папке:

```bash
cp /usr/lib/x86_64-linux-gnu/libdav1d.so.7 appdata/libs/
patchelf --set-rpath '$ORIGIN' appdata/libs/libmpv.so
```

`$ORIGIN` — это каталог самой библиотеки, так что связка переезжает целиком.
А если в `NEEDED` попал абсолютный путь (та самая беда с mujs), его сначала надо
превратить в обычное имя:

```bash
patchelf --replace-needed /usr/lib/libmujs.so libmujs.so.1 appdata/libs/libmpv.so
```

Но правильнее такую фичу просто выключить при сборке — тем эта инструкция и
занимается.
