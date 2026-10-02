lines = (0...200_000).map { |i| "item #{i}: #{i * i} #{i % 7 == 0 ? "lucky" : "plain"}" }
text = lines.join("\n")
total = text.split("\n").sum { |line| line.split(" ").sum(&:size) }
puts text.size
puts total
