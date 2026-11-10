# CineUSB

Biblioteca USB para LG webOS, con la interfaz nativa de
[PlxNative](https://github.com/GLinnik21/plx-native) como base. El identificador de
instalación es `com.seergi893.cineusb`. La compilación para webOS abre el
explorador local directamente, sin iniciar la sesión de Plex.

La cuadrícula usa las tarjetas y animaciones de PlxNative. Con las flechas
eliges un disco, carpeta o vídeo, con OK lo abres y con Atrás vuelves al nivel
anterior. Los archivos conservan sus nombres y la estructura de carpetas del disco.
Los vídeos MKV, MP4, M4V, MOV, WebM, TS, M2TS y MTS se abren en el motor Feed con el
HUD clásico integrado. La compatibilidad de cada pista depende del códec y el
decodificador de la televisión.

Si el disco aparece en la carpeta de webOS pero el aislamiento oculta su
montaje, CineUSB solicita al servicio local de Homebrew Channel con root un
montaje de solo lectura en su propio entorno. La aplicación comprueba el
resultado antes de mostrar el volumen. El servicio vuelve a comprobar los
montajes al conectar o retirar discos mientras se navega por la biblioteca.

## Compilar para webOS

En un host compatible con el SDK del proyecto, ejecuta:

```sh
make setup-env
make -j4 FLAVOR=stable RELEASE=1 all
make FLAVOR=stable RELEASE=1 ipk
```

El resultado estará en `pkg/com.seergi893.cineusb_*_arm.ipk`. En GitHub, el
workflow **Build and publish CineUSB** usa un runner Linux ARM. La publicación
incluye el paquete, su suma SHA-256, el manifiesto de Homebrew y el código
fuente correspondiente.

## Probar en la LG

Instala el IPK en la LG con Homebrew Channel y root. Conecta el disco, abre
CineUSB y pulsa OK sobre la unidad. Entra en una carpeta y reproduce un MP4
H.264/AAC y un MKV. Comprueba pausa, avance y vuelta a la misma carpeta con
Atrás. Luego retira el disco, espera a que desaparezca de la cuadrícula y
vuelve a conectarlo con la app abierta.

Si ves el aviso de montaje, pulsa OK para reintentar. Para diagnosticarlo,
compara los montajes que ve el proceso de CineUSB con `/tmp/usb` en el sistema.

## Código y licencia

Base `1c4d4f51f64a8dd66127a11db57834a1013af265` de PlxNative.
Se conservan los avisos de autoría, la licencia GPL-3.0-or-later y las
licencias de terceros. Consulta `LICENSING.md` antes de distribuir un binario.
