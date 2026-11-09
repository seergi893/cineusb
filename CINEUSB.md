# CineUSB

Prototipo de exploración USB para LG webOS, con la interfaz nativa de
[PlxNative](https://github.com/GLinnik21/plx-native) como base. El identificador de
instalación es `com.seergi893.cineusb`. La compilación para webOS abre el
explorador local directamente, sin iniciar la sesión de Plex.

La primera prueba muestra los discos que el proceso puede ver y permite
navegar por las subcarpetas. Al elegir un MKV, MP4 o M4V, lee el primer y el
último byte para confirmar que puede abrir y recorrer el archivo. La prueba
trabaja en modo de solo lectura. La reproducción se conectará después a la
ruta nativa, una vez confirmado el acceso de la aplicación al USB.

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

Instala el IPK, conecta un disco, abre CineUSB y pulsa OK sobre una unidad.
Entra en una subcarpeta y elige un vídeo. El mensaje «Lectura correcta»
confirma que el proceso de CineUSB puede acceder a los datos del archivo.
Prueba también a retirar y volver a conectar el disco. Si CineUSB no lo
lista, el primer diagnóstico es comparar las rutas visibles en su
entorno de aplicación con las que webOS expone al sistema.

## Código y licencia

Base `1c4d4f51f64a8dd66127a11db57834a1013af265` de PlxNative.
Se conservan los avisos de autoría, la licencia GPL-3.0-or-later y las
licencias de terceros. Consulta `LICENSING.md` antes de distribuir un binario.
