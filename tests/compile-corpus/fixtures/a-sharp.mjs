import sharp from "sharp";
const b=await sharp({create:{width:4,height:4,channels:3,background:"#f00"}}).png().toBuffer();
console.log("ok:" + (b.length>0));
