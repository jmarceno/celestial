#pragma once

#include "rust/cxx.h"
#include <QGuiApplication>
#include <QIcon>
#include <QImage>
#include <QPainter>
#include <QPixmap>
#include <QSvgRenderer>
#include <stdexcept>

namespace celestial::branding {

inline QImage logo_image(rust::Slice<const std::uint8_t> svg, int size) {
    QSvgRenderer renderer(QByteArray(reinterpret_cast<const char *>(svg.data()), svg.size()));
    if (!renderer.isValid() || size <= 0 || size > 256) {
        throw std::runtime_error("Invalid Celestial logo or icon size");
    }
    QImage image(size, size, QImage::Format_ARGB32);
    image.fill(Qt::transparent);
    QPainter painter(&image);
    renderer.render(&painter);
    return image;
}

// StatusNotifier expects straight-alpha ARGB in network byte order.
inline rust::Vec<std::uint8_t> logo_argb(rust::Slice<const std::uint8_t> svg, int size) {
    const auto image = logo_image(svg, size);
    rust::Vec<std::uint8_t> data;
    data.reserve(size * size * 4);
    for (int y = 0; y < size; ++y) {
        for (int x = 0; x < size; ++x) {
            const auto pixel = image.pixel(x, y);
            data.push_back(qAlpha(pixel));
            data.push_back(qRed(pixel));
            data.push_back(qGreen(pixel));
            data.push_back(qBlue(pixel));
        }
    }
    return data;
}

inline void set_window_icon(rust::Slice<const std::uint8_t> svg) {
    QIcon icon;
    for (const auto size : {16, 24, 32, 48, 64, 128, 256}) {
        icon.addPixmap(QPixmap::fromImage(logo_image(svg, size)));
    }
    QGuiApplication::setWindowIcon(icon);
}

} // namespace celestial::branding
